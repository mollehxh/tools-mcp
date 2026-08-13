use std::time::Duration;

#[derive(Clone, Debug)]
pub struct InstallLimits {
    pub max_files: usize,
    pub max_file_bytes: usize,
    pub max_materialized_bytes: usize,
    pub max_path_bytes: usize,
    pub max_path_depth: usize,
    pub max_segment_bytes: usize,
    pub max_total_path_bytes: usize,
    pub max_transport_bytes: usize,
    pub max_objects: usize,
    pub max_object_bytes: usize,
    pub max_expanded_object_bytes: usize,
    pub timeout: Duration,
}

impl Default for InstallLimits {
    fn default() -> Self {
        Self {
            max_files: 1_024,
            max_file_bytes: 4 * 1024 * 1024,
            max_materialized_bytes: 32 * 1024 * 1024,
            max_path_bytes: 4 * 1024,
            max_path_depth: 64,
            max_segment_bytes: 255,
            max_total_path_bytes: 4 * 1024 * 1024,
            max_transport_bytes: 64 * 1024 * 1024,
            max_objects: 16_384,
            max_object_bytes: 8 * 1024 * 1024,
            max_expanded_object_bytes: 64 * 1024 * 1024,
            timeout: Duration::from_secs(30),
        }
    }
}
