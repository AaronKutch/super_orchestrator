use std::collections::BTreeMap;

use crate::{
    acquire_dir_path, acquire_file_path, Command, CommandRunner, Error, MapAddError, Result,
};

pub struct Container {
    pub name: String,
    pub image: String,
    // each string is passed in as `--build-arg "[String]"` (the quotations are added), so a string
    // "ARG=val" would set the variable "ARG" for the docker file to use.
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
    container_runners: BTreeMap<String, CommandRunner>,
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
            container_runners: BTreeMap::new(),
        }
    }

    // just apply `rm -f` to all containers, ignoring errors
    async fn unconditional_terminate(&mut self) {
        while let Some((_, id)) = self.active_container_ids.pop_first() {
            let _ = Command::new("docker", &["rm", "-f", &id])
                .run_to_completion()
                .await;
        }
    }

    /// Force removes all containers
    pub async fn terminate_all(mut self) -> Result<()> {
        while let Some(entry) = self.active_container_ids.first_entry() {
            let comres = Command::new("docker", &["rm", "-f", entry.get()])
                .run_to_completion()
                .await
                .map_add_err("terminate_all -> ");
            if let Err(e) = comres {
                // in case this is some weird one-off problem, we do not want to leave a whole
                // network running
                self.unconditional_terminate().await;
                return Err(e)
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
        let log_dir = acquire_dir_path(&self.log_dir)
            .await?
            .to_str()
            .ok_or_else(|| {
                Error::from(format!(
                    "ContainerNetwork::run() -> log_dir: \"{}\" could not be canonicalized into a \
                     String",
                    self.log_dir
                ))
            })?
            .to_owned();
        for container in &self.containers {
            acquire_file_path(&container.entrypoint_path).await?;
        }

        // remove old network if it exists (there is no option to ignore nonexistent
        // networks, drop exit status errors and let the creation command handle any
        // higher order errors)
        let _ = Command::new("docker", &["network", "rm", &self.network_name])
            .run_to_completion()
            .await;
        let comres = if self.is_not_internal {
            Command::new("docker", &["network", "create", &self.network_name])
                .run_to_completion()
                .await?
        } else {
            Command::new("docker", &[
                "network",
                "create",
                "--internal",
                &self.network_name,
            ])
            .run_to_completion()
            .await?
        };
        // TODO we can get the network id
        comres.assert_success()?;

        // run all the creation first so that everything is pulled and prepared
        for container in &self.containers {
            let bin_path = acquire_file_path(&container.entrypoint_path).await?;
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
            let mut tmp = vec![];
            for arg in &container.entrypoint_args {
                tmp.push(format!("\"{arg}\""));
            }
            for s in &tmp {
                args.push(s);
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
                            self.unconditional_terminate().await;
                            return Err(e)
                        }
                    }
                }
                Err(e) => {
                    self.unconditional_terminate().await;
                    return Err(e.add_error("{self:?}.run() -> "))
                }
            }
        }

        // start all containers
        for (container_name, id) in self.active_container_ids.clone().iter() {
            let mut command = Command::new("docker", &["start", "--attach", id]);
            command.stderr_file = Some(format!("{}/container_{}_err.log", log_dir, container_name));
            match command.run().await {
                Ok(runner) => {
                    self.container_runners
                        .insert(container_name.clone(), runner);
                }
                Err(e) => {
                    self.unconditional_terminate().await;
                    return Err(e)
                }
            }
        }

        Ok(())
    }
}
