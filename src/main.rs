#[cfg(not(target_os = "windows"))]
compile_error!("This project only supports Windows.");

mod partition;

use partition::Partition;
use std::result::Result;

fn main() -> Result<(), String> {
    println!("Hello, world!");

    let partition = Partition::new()?;

    partition.configure(2)?;

    partition.setup()?;
    partition.create_vp(0)?;
    partition.create_vp(1)?;

    partition.allocate_memory()?;

    println!("Virtual processors created");

    partition.delete()?;
    
    std::process::exit(0)
}
