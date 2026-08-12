mod app;
mod mdns;
mod pairing;

use plato_core::anyhow::Error;
use crate::app::run;

fn main() -> Result<(), Error> {
    run()?;
    Ok(())
}
