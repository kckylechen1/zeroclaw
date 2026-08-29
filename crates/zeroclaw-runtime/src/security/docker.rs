//! Docker sandbox (container isolation)

use crate::security::traits::Sandbox;
use std::path::PathBuf;
use std::process::Command;

/// Docker sandbox backend
#[derive(Debug, Clone)]
pub struct DockerSandbox {
    image: String,
    workspace_dir: Option<PathBuf>,
}

impl Default for DockerSandbox {
    fn default() -> Self {
        Self {
            image: "alpine:latest".to_string(),
            workspace_dir: None,
        }
    }
}

impl DockerSandbox {
    /// Default container image used when no explicit image is configured.
    /// Exposed so callers constructing via with_workspace() without a custom
    /// image don't duplicate the default-image string.
    pub fn default_image() -> String {
        Self::default().image
    }

    /// Construct a Docker sandbox with a workspace bind-mount (read-only).
    /// Used by Python/R/Julia skills that need to access script files from
    /// the workspace inside the container.
    pub fn with_workspace(image: String, workspace_dir: PathBuf) -> std::io::Result<Self> {
        if Self::is_installed() {
            Ok(Self {
                image,
                workspace_dir: Some(workspace_dir),
            })
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Docker not found",
            ))
        }
    }

    pub fn new() -> std::io::Result<Self> {
        if Self::is_installed() {
            Ok(Self::default())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Docker not found",
            ))
        }
    }

    pub fn with_image(image: String) -> std::io::Result<Self> {
        if Self::is_installed() {
            Ok(Self {
                image,
                workspace_dir: None,
            })
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Docker not found",
            ))
        }
    }

    pub fn probe() -> std::io::Result<Self> {
        Self::new()
    }

    fn is_installed() -> bool {
        Command::new("docker")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

impl Sandbox for DockerSandbox {
    fn wrap_command(&self, cmd: &mut Command) -> std::io::Result<()> {
        let program = cmd.get_program().to_string_lossy().to_string();
        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        let mut docker_cmd = Command::new("docker");
        docker_cmd.args([
            "run",
            "--rm",
            "--memory",
            "512m",
            "--cpus",
            "1.0",
            "--network",
            "none",
        ]);

        if let Some(workspace) = &self.workspace_dir {
            let workspace_str = workspace.to_string_lossy();
            docker_cmd.arg("-v");
            docker_cmd.arg(format!("{workspace_str}:{workspace_str}:ro"));
            docker_cmd.arg("--workdir");
            docker_cmd.arg(workspace_str.as_ref());
        }

        docker_cmd.arg(&self.image);
        docker_cmd.arg(&program);
        docker_cmd.args(&args);

        *cmd = docker_cmd;
        Ok(())
    }

    fn is_available(&self) -> bool {
        Self::is_installed()
    }

    fn name(&self) -> &str {
        "docker"
    }

    fn description(&self) -> &str {
        "Docker container isolation (requires docker)"
    }
}
