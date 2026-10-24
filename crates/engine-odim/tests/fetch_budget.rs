//! The PVOL scan downloads new volumes under the background fetch budget
//! (#1011, #1055): a GET whose body stalls past the request budget still
//! adds its volume, where the request budget would drop it until the next
//! poll. Over a store whose GET bodies stall, on a paused clock
//! (`ds_storage::test_util`).

use ds_storage::test_util::{run_stalled, StalledBody};

/// Two sweeps of `DBZH` and `VRAD` over 8 km from site `test`, timed
/// 2026-09-16 12:00 UTC.
const FIXTURE: &[u8] = include_bytes!("../../../testdata/pvol-cold-batch.h5");

#[test]
fn the_volume_scan_downloads_under_the_background_budget() {
    let stalled = StalledBody::with_objects([("budget/202609161200_test_PVOL.h5", FIXTURE)]);
    let store = ds_storage::DataStore::new(stalled.clone());
    let sites = run_stalled(&stalled, move || {
        engine_odim::PolarVolumeEngine::new_remote_for_test("fetch-budget", store, "budget/")
            .expect("scan the store")
            .sites()
            .into_iter()
            .map(|(nod, _)| nod)
            .collect::<Vec<_>>()
    });
    assert_eq!(stalled.stalled(), 1, "the volume's download stalled");
    assert_eq!(sites, ["test"], "the stalled volume is catalogued");
}
