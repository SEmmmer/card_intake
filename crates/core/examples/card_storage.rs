//! Prints what Windows' storage stack reports about a card, and its NVMe health log
//! when the card is a native NVMe drive. Nothing is written to the card.
//!
//!     cargo run -p card_intake_core --example card_storage -- E:\

fn main() {
    let root = std::env::args()
        .nth(1)
        .expect("usage: card_storage <card root>");
    let root = std::path::Path::new(&root);
    println!("{:#?}", card_intake_core::storage::identify(root));
    println!("{:#?}", card_intake_core::storage::read_nvme_health(root));
}
