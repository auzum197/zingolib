//! This is a mod for data structs that will be used across all sections of zingolib.

/// Return type for fns that poll the status of task handles.
pub enum PollReport<T, E> {
    /// Task has not been launched.
    NoHandle,
    /// Task is not complete.
    NotReady,
    /// Task has completed successfully or failed.
    Ready(Result<T, E>),
}
