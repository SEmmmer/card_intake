//! Prints what Windows' storage stack reports about a card, and its NVMe health log
//! when the app can read it without asking: on the NVMe bus, or through the reader's
//! JMicron bridge when run as admin. Nothing is written to the card.
//!
//!     cargo run -p card_intake_core --example card_storage -- E:\

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(code) = card_intake_core::health::serve_admin_request(&args) {
        std::process::exit(code);
    }
    let root = args.get(1).expect("usage: card_storage <card root>");
    let root = std::path::Path::new(root);
    println!("{:#?}", card_intake_core::storage::identify(root));
    println!("{:#?}", card_intake_core::card_details::card_details(root));
    println!("{:#?}", card_intake_core::storage::read_nvme_health(root));
}
