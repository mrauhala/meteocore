//! Quick reflectivity-content probe for a polar volume, to judge which
//! fixture timestep has "interesting" weather worth a 3D-tiles demo.
//!
//! Usage: cargo run -p engine-odim --example volume_stats -- <file.h5> [QUANTITY]
//! QUANTITY defaults to DBZH (falls back to TH if a sweep lacks DBZH).

use engine_odim::pvol::{read_moment_pixels, read_polar_volume};
use std::fs;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: volume_stats <file.h5> [QUANTITY]");
    let want = args.next().unwrap_or_else(|| "DBZH".to_string());

    let bytes = fs::read(&path).expect("read file");
    let vol = read_polar_volume(&bytes).expect("parse volume");

    println!("file:   {path}");
    println!(
        "site:   nod={:?} plc={:?}  lon={:.4} lat={:.4} h={:.0}m",
        vol.site.nod, vol.site.plc, vol.site.lon, vol.site.lat, vol.site.height
    );
    println!("time:   {}  object={}", vol.time, vol.object);
    println!("sweeps: {}", vol.sweeps.len());
    println!();
    println!(
        "{:>3}  {:>6}  {:>5}x{:>4}  {:>7}  {:>6}  {:>6}  {:>6}  {:>6}",
        "sw", "elang", "rays", "bins", "valid%", "maxdBZ", ">=20%", ">=35%", ">=50%"
    );

    // Volume-wide accumulators.
    let mut vmax = f64::NEG_INFINITY;
    let mut vtot: u64 = 0;
    let mut vvalid: u64 = 0;
    let mut v35: u64 = 0;
    let mut v50: u64 = 0;
    // Approximate echo top: highest beam-center altitude with a >=35 dBZ cell.
    let mut echo_top_m = 0.0_f64;
    const FOUR_THIRDS_EARTH_M: f64 = 4.0 / 3.0 * 6_371_000.0;

    for (i, sw) in vol.sweeps.iter().enumerate() {
        // Pick the requested quantity, fall back to TH.
        let mom = sw
            .moments
            .iter()
            .find(|m| m.quantity == want)
            .or_else(|| sw.moments.iter().find(|m| m.quantity == "TH"));
        let Some(mom) = mom else {
            println!("{i:>3}  {:>6.2}  (no {want}/TH)", sw.elangle);
            continue;
        };
        let px = match read_moment_pixels(&bytes, &mom.dataset_path, sw.nrays, sw.nbins) {
            Ok(p) => p,
            Err(e) => {
                println!("{i:>3}  {:>6.2}  read err: {e:?}", sw.elangle);
                continue;
            }
        };

        let mut smax = f64::NEG_INFINITY;
        let mut valid: u64 = 0;
        let mut c20: u64 = 0;
        let mut c35: u64 = 0;
        let mut c50: u64 = 0;
        let total = (sw.nrays * sw.nbins) as u64;
        for ray in 0..sw.nrays {
            for bin in 0..sw.nbins {
                if let Some(v) = px.sample(
                    ray,
                    bin,
                    mom.gain,
                    mom.offset,
                    mom.nodata,
                    Some(mom.undetect),
                ) {
                    valid += 1;
                    if v > smax {
                        smax = v;
                    }
                    if v >= 20.0 {
                        c20 += 1;
                    }
                    if v >= 35.0 {
                        c35 += 1;
                        // Beam-center height for this bin (4/3-earth model).
                        let r = sw.rstart + (bin as f64 + 0.5) * sw.rscale;
                        let el = sw.elangle.to_radians();
                        let rp = FOUR_THIRDS_EARTH_M;
                        let h = (r * r + rp * rp + 2.0 * r * rp * el.sin()).sqrt() - rp;
                        if h > echo_top_m {
                            echo_top_m = h;
                        }
                    }
                    if v >= 50.0 {
                        c50 += 1;
                    }
                }
            }
        }
        let pct = |c: u64| 100.0 * c as f64 / total as f64;
        println!(
            "{i:>3}  {:>6.2}  {:>5}x{:>4}  {:>6.1}%  {:>6.1}  {:>5.2}%  {:>5.2}%  {:>5.2}%",
            sw.elangle,
            sw.nrays,
            sw.nbins,
            100.0 * valid as f64 / total as f64,
            if smax.is_finite() { smax } else { f64::NAN },
            pct(c20),
            pct(c35),
            pct(c50),
        );

        vmax = vmax.max(smax);
        vtot += total;
        vvalid += valid;
        v35 += c35;
        v50 += c50;
    }

    println!();
    println!("VOLUME SUMMARY ({want}):");
    if vvalid == 0 {
        // No valid samples (no readable sweeps, or every cell nodata):
        // vmax is still -inf and coverage would be 0/0 = NaN.
        println!("  no valid {want}/TH samples ({vtot} cells, all nodata/undetect)");
        return;
    }
    println!("  max reflectivity : {vmax:.1} dBZ");
    println!(
        "  valid coverage   : {:.1}% of {vtot} cells",
        100.0 * vvalid as f64 / vtot as f64
    );
    println!("  convective (>=35): {v35} cells");
    println!("  heavy core (>=50): {v50} cells");
    println!(
        "  approx echo top  : {:.1} km (highest >=35 dBZ beam center)",
        echo_top_m / 1000.0
    );
}
