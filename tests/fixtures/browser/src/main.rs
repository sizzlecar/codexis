fn main() {
    greet();
}

fn greet() {
    println!("hello");
}

pub struct Config;

pub trait Engine {
    fn infer(&self);
}

impl Engine for Config {
    fn infer(&self) {}
}
