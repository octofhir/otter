fn main() {
    let slot_size = std::mem::size_of::<otter_gc::typed::TypedHeap>();
    println!("TypedHeap size: {}", slot_size);
    // I can't easily access the internal Slot type from here.
}
