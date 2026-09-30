#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
compile_error!("gatekv targets Linux x86-64 only: its assembly needs SSE4.2 and ELF directives");

// x86-64 being the only target, usize is 64 bits wide: `as` between usize and u64, or from
// u32, never truncates. Every other conversion is checked.

pub mod asm;
pub mod kv;
pub mod raft;
pub mod resp;
pub mod store;
pub mod wire;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Invalid(&'static str),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
