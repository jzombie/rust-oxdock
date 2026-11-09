use anyhow::Context;

#[allow(clippy::disallowed_types)]
fn main() -> anyhow::Result<()> {
    let script = std::io::read_to_string(std::io::stdin()).context("read child script")?;
    oxdock_render::run_child_script(&script)
}
