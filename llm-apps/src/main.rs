//! Step 2: the runtime, before the weights.
//!
//! Nothing here loads a model. It builds two tensors, multiplies them on the
//! CPU, and prints what came back - proof that candle works on this machine
//! before step 3 hands it a real embedding model.

use candle_core::{Device, Tensor};

fn main() -> candle_core::Result<()> {
    let device = Device::Cpu;

    let a = Tensor::arange(0f32, 6f32, &device)?.reshape((2, 3))?;
    let b = Tensor::arange(0f32, 6f32, &device)?.reshape((3, 2))?;
    let c = a.matmul(&b)?;

    println!("running on: {device:?}");
    println!("a = {a}");
    println!("a x b = {c}");

    Ok(())
}
