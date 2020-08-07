use std::error::Error;
use jwalk::{WalkDir};

fn main() -> Result<(), Box<dyn Error>> {
    for entry in WalkDir::new(".").sort(true) {
      println!("{}", entry?.path().display());
    }

    Ok(())
}
