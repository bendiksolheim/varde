// Phase 1: load + validate only. Runtime wiring (state, loops, server) arrives in Phase 3.
fn main() {
    let path = varde::config::config_path();
    match varde::config::load(&path) {
        Ok((config, warnings)) => {
            for warning in warnings {
                eprintln!("{warning}");
            }
            println!("config OK: {} services", config.services.len());
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
