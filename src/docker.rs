use std::collections::BTreeMap;

use crate::{acquire_dir_path, acquire_file_path, Command, Error, Result};

pub struct Container {
    pub name: String,
    pub image: String,
    // each string is passed in as `--build-arg "[String]"` (the quotations are added), so a string "ARG=val" would set the variable "ARG" for the docker file to use.
    pub build_args: Vec<String>,
    // path to the entrypoint binary locally
    pub entrypoint_path: String,
    // passed in as ["arg1", "arg2", ...] with the bracket and quotations being added
    pub entrypoint_args: Vec<String>,
}

#[must_use]
pub struct ContainerNetwork {
    network_name: String,
    containers: Vec<Container>,
    /// is `--internal` by default
    is_not_internal: bool,
    log_dir: String,
    active_container_ids: BTreeMap<String, String>,
}

impl ContainerNetwork {
    pub fn new(
        network_name: String,
        containers: Vec<Container>,
        is_not_internal: bool,
        log_dir: String,
    ) -> Self {
        Self {
            network_name,
            containers,
            is_not_internal,
            log_dir,
            active_container_ids: BTreeMap::new(),
        }
    }

    // just apply `rm -f` to all containers, ignoring errors
    async fn unconditional_terminate(mut self) {
        while let Some((_, id)) = self.active_container_ids.pop_first() {
            let _ = Command::new("docker", &["rm", "-f", &id])
                .run_to_completion()
                .await;
        }
    }

    /// Force removes all containers
    pub async fn terminate_all(mut self) -> Result<()> {
        while let Some(entry) = self.active_container_ids.first_entry() {
            let comres = Command::new("docker", &["rm", "-f", &entry.get()])
                .run_to_completion()
                .await
                .map_err(|e| e.generic_error("terminate_all -> "));
            if let Err(e) = comres {
                // in case this is some weird one-off problem, we do not want to leave a whole network running
                self.unconditional_terminate();
                return Err(e);
            }
            // ignore status failures, because the container is probably already gone
            // TODO there is maybe some error message parsing we should do

            // only pop from `container_ids` after success
            self.active_container_ids.pop_first().unwrap();
        }
        Ok(())
    }

    pub async fn run(&mut self) -> Result<()> {
        // preverification to prevent much more expensive later container creation undos
        acquire_dir_path(&self.log_dir).await?;
        for container in &self.containers {
            acquire_file_path(&container.entrypoint_path).await?;
        }

        // remove old network if it exists (there is no option to ignore nonexistent
        // networks, drop exit status errors and let the creation command handle any higher order errors)
        let _ = Command::new("docker", &["network", "rm", &self.network_name])
            .run_to_completion()
            .await;
        let args: &[&str] = if self.is_not_internal {
            &["network", "create", &self.network_name]
        } else {
            &["network", "create", "--internal", &self.network_name]
        };
        let comres = Command::new("docker", args).run_to_completion().await?;
        // TODO we can get the network id
        comres.assert_success()?;

        // run all the creation first so that everything is pulled and prepared
        for container in &self.containers {
            let bin_path = acquire_file_path(&container.entrypoint_path).await?;
            let log_dir = acquire_dir_path(&self.log_dir).await?;
            let bin_s = bin_path.file_name().unwrap().to_str().unwrap();
            // just include the needed binary
            let volume = format!("{}:/usr/bin/{}", container.entrypoint_path, bin_s);
            let mut args = vec![
                "create",
                "--rm",
                "--network",
                &self.network_name,
                "--hostname",
                &container.name,
                "--name",
                &container.name,
                "--volume",
                &volume,
            ];
            args.push(&container.image);
            args.push(bin_s);
            // TODO
            for arg in &container.entrypoint_args {
                args.push(&format!("\"{arg}\""));
            }
            /*if !container.entrypoint_args.is_empty() {
                let mut s = "[";

                for (i, arg) in container.entrypoint_args.iter().enumerate() {
                    args += "\"";
                    args += "\"";
                }
                args.push(&container.entrypoint_args);
                s += "]";
            }*/
            match Command::new("docker", &args).run_to_completion().await {
                Ok(output) => {
                    match output.assert_success() {
                        Ok(_) => {
                            let mut id = output.stdout;
                            // remove trailing '\n'
                            id.pop().unwrap();
                            self.active_container_ids.insert(container.name.clone(), id);
                        }
                        Err(e) => {
                            self.unconditional_terminate();
                            return Err(e);
                        }
                    }
                }
                Err(e) => {
                    self.unconditional_terminate();
                    return Err(e.generic_error("{self:?}.run() -> "));
                }
            }
        }

        // start all containers
        let mut ccs = vec![];
        for (container_name, id) in active_container_ids.clone().iter() {
            let args = vec!["start", "--attach", id];
            let stderr = acquire_dir_path(&self.log_dir)
                .await?
                .join(format!("container_{}_err.log", container_name));
            let cc = Command::new("docker", &args, ci)
                .unwrap()
                .stderr_to_file(&stderr)
                .await
                .unwrap();
            ccs.push(cc);
        }

        let cc = ccs.pop().unwrap();
        // wait on last container finishing
        print!("waiting on last container... ",);
        match cc.wait().await {
            Ok(()) => {
                println!("done");
            }
            Err(e) => {
                println!("force stopping all containers: {}\n", e);
                force_stop_containers(&mut active_container_ids);
                return Err(Error::from("failed when waiting on last container"));
            }
        }

        force_stop_containers(&mut active_container_ids);
        Ok(())
    }
}
