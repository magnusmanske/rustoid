fn main() {
    let stamp = "January 2020";
    let (date_part, time_part) = match stamp.split_once(['T', ' ']) {
        Some((d, t)) => (d.trim(), Some(t.trim())),
        None => (stamp, None),
    };
    println!("date_part={:?} time_part={:?}", date_part, time_part);
}
