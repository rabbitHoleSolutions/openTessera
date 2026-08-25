use tessera_mpv::{Mpv, MpvLib};

fn main() {
    let lib = MpvLib::load().expect("libmpv");
    let mpv = Mpv::new(&lib, &[("vo", "null"), ("ao", "null"), ("idle", "yes")]).expect("mpv");

    println!("initial mute: {:?}", mpv.get_string("mute"));

    let r1 = mpv.command(&["set_property", "mute", "no"]);
    println!("command set_property -> {:?}", r1.map_err(|e| e.to_string()));
    println!("mute after set_property: {:?}", mpv.get_string("mute"));

    let r2 = mpv.command(&["set", "mute", "no"]);
    println!("command set -> {:?}", r2.map_err(|e| e.to_string()));
    println!("mute after set: {:?}", mpv.get_string("mute"));

    let r3 = mpv.command(&["set", "pause", "yes"]);
    println!("command set pause -> {:?}", r3.map_err(|e| e.to_string()));
    println!("pause after set: {:?}", mpv.get_string("pause"));
}
