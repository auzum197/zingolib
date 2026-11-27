use std::{env, path::Path};

/// Checks if zcash params are available and downloads them if not.
/// Also copies them to `zcash-params` for the testutils prover to embed.
fn get_zcash_params() {
    println!("Checking if params are available...");

    let params_path = match zcash_proofs::download_sapling_parameters(Some(400)) {
        Ok(p) => {
            println!("Params downloaded!");
            println!("Spend path: {}", p.spend.to_str().unwrap());
            println!("Output path: {}", p.output.to_str().unwrap());
            p
        }
        Err(e) => {
            println!("Error downloading params: {e}");
            panic!();
        }
    };

    let internal_params_path = Path::new("zcash-params");
    std::fs::create_dir_all(internal_params_path).unwrap();
    std::fs::copy(
        params_path.spend,
        internal_params_path.join("sapling-spend.params"),
    )
    .unwrap();

    std::fs::copy(
        params_path.output,
        internal_params_path.join("sapling-output.params"),
    )
    .unwrap();
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // only the test harness proves transactions
    if env::var_os("CARGO_FEATURE_TESTUTILS").is_some() {
        get_zcash_params();
    }
    Ok(())
}
