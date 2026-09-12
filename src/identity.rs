//! Compile-time identity: optimization profile never selects the app channel.
pub const IS_DEV: bool = cfg!(feature = "dev");
pub const NAME: &str = if IS_DEV { "Gopher Dev" } else { "Gopher" };
pub const HEADER: &str = if IS_DEV { "Gopher • Dev" } else { "Gopher" };
pub const DATA_DIRECTORY: &str = if IS_DEV {
    ".config/gopher-dev"
} else {
    ".config/gopher"
};
pub const QUIT: &str = if IS_DEV {
    "Quit Gopher Dev"
} else {
    "Quit Gopher"
};
pub const OPEN: &str = if IS_DEV {
    "Open Gopher Dev"
} else {
    "Open Gopher"
};

pub fn version_label() -> String {
    let version = env!("CARGO_PKG_VERSION");
    if IS_DEV {
        format!(
            "{NAME} {version}\n{} · {}",
            env!("GOPHER_BUILD_REVISION"),
            env!("GOPHER_BUILD_STATUS")
        )
    } else {
        format!("Updates · {NAME} {version}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn channel_selects_data_and_display_identity_together() {
        assert!(
            crate::config::Config::directory()
                .unwrap()
                .ends_with(DATA_DIRECTORY)
        );
        if cfg!(feature = "dev") {
            assert_eq!(NAME, "Gopher Dev");
            assert_eq!(HEADER, "Gopher • Dev");
            assert_eq!(DATA_DIRECTORY, ".config/gopher-dev");
            assert!(version_label().contains(env!("GOPHER_BUILD_REVISION")));
        } else {
            assert_eq!(NAME, "Gopher");
            assert_eq!(HEADER, "Gopher");
            assert_eq!(DATA_DIRECTORY, ".config/gopher");
            assert_eq!(
                version_label(),
                format!("Updates · Gopher {}", env!("CARGO_PKG_VERSION"))
            );
        }
    }
}
