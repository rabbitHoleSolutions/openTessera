use tessera_mpv::{Mpv, MpvLib};

fn main() {
    let lib = MpvLib::load().expect("libmpv");
    let mpv = Mpv::new(&lib, &[("vo", "null"), ("ao", "null"), ("idle", "yes")]).expect("mpv");
    let file = std::env::args().nth(1).expect("usage: async_test FILE");
    mpv.load_file(&file, "replace").expect("load");
    std::thread::sleep(std::time::Duration::from_millis(2000));
    println!("time-pos: {:?}", mpv.get_f64("time-pos"));
    println!("sending async seek...");
    let r = mpv.command_async(&["seek", "10", "absolute"]);
    println!("command_async -> {:?}", r.map_err(|e| e.to_string()));
    std::thread::sleep(std::time::Duration::from_millis(1500));
    println!("time-pos after: {:?}", mpv.get_f64("time-pos"));
    // drain events to make sure replies parse fine
    for _ in 0..10 {
        if let Some(ev) = mpv.poll_event() {
            println!("event: {ev:?}");
        }
    }
    println!("done");
}
