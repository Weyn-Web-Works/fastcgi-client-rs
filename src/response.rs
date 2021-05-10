use std::{collections::HashMap, fmt, fmt::Debug};

pub(crate) type ResponseMap = HashMap<u16, Response>;

/// Output of fastcgi request, contains STDOUT and STDERR.
#[derive(Default, Clone)]
pub struct Response {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        Debug::fmt(r#"Output { stdout: "...", stderr: "..." }"#, f)
    }
}
