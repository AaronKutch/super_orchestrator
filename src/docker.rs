use std::{collections::BTreeMap};

use crate::{acquire_dir_path, acquire_file_path, Command, Error};

/*
pub fn stop_containers(active_container_ids: &mut BTreeMap<String, String>) {
    for (name, id) in active_container_ids.iter() {
        let rm_output = Command::new("docker").args(["rm", id]).output().unwrap();
        if rm_output.status.success() {
            println!("stopped container {}", name);
        } else {
            println!("tried to stop container {} and got {:?}", name, rm_output);
        }
    }
    active_container_ids.clear();
}

pub fn force_stop_containers(active_container_ids: &mut BTreeMap<String, String>) {
    for (name, id) in active_container_ids.iter() {
        let rm_output = Command::new("docker")
            .args(["rm", "-f", id])
            .output()
            .unwrap();
        if rm_output.status.success() {
            println!("force stopped container {}", name);
        } else {
            println!(
                "tried to force stop container {} and got {:?}",
                name, rm_output
            );
        }
    }
    active_container_ids.clear();
}*/

pub struct Container {
    pub name: String,
    pub image: String,
    pub bin_path: String,
    pub extra_args: String,
}

pub struct ContainerNetwork {
    pub network_name: String,
    pub containers: Vec<Container>,
    /// is `--internal` by default
    pub is_not_internal: bool,
    pub log_dir: String,
}

impl ContainerNetwork {
    pub async fn run(&mut self, ci_mode: bool) -> Result<(), Error> {
        let ci: bool = ci_mode;
        acquire_dir_path(&self.log_dir).await?;
        for container in &self.containers {
            acquire_file_path(&container.bin_path).await?;
        }

        // create an `--internal` docker network
        println!("creating docker network {}", self.network_name);
        // remove old network if it exists (there is no option to ignore nonexistent
        // networks, drop exit status errors)
        let _ = Command::new("docker", &["network", "rm", &self.network_name], ci)
            .unwrap()
            .wait()
            .await;
        // create the network as `--internal`
        Command::new(
            "docker",
            &["network", "create", "--internal", &self.network_name],
            ci,
        )
        .unwrap()
        .wait_for_output()
        .await
        .unwrap();

        // run all the creation first so that everything is pulled and prepared
        let mut active_container_ids: BTreeMap<String, String> = BTreeMap::new();
        for container in &self.containers {
            let bin_path = acquire_file_path(&container.bin_path).await?;
            let log_dir = acquire_dir_path(&self.log_dir).await?;
            let bin_s = bin_path.file_name().unwrap().to_str().unwrap();
            // just include the needed binary
            let volume = format!("{}:/usr/bin/{}", container.bin_path, bin_s);
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
                &container.image,
                bin_s,
            ];
            if !container.extra_args.is_empty() {
                args.push(&container.extra_args);
            }
            match Command::new("docker", &args, ci)
                .unwrap()
                .stderr_to_file(&log_dir.join("cmd_docker_create_err.log"))
                .await
                .unwrap()
                .wait_for_output()
                .await
            {
                Ok(output) => {
                    let mut id = output.stdout;
                    // remove trailing '\n'
                    id.pop().unwrap();
                    println!("created container {}", &container.name);
                    active_container_ids.insert(container.name.clone(), id);
                }
                Err(e) => {
                    println!("force stopping all containers: {}\n", e);
                    force_stop_containers(&mut active_container_ids);
                    return Err(Error::from("failed when creating container"))
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
                return Err(Error::from("failed when waiting on last container"))
            }
        }

        force_stop_containers(&mut active_container_ids);
        Ok(())
    }
}
