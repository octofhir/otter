use otter_gc::HeapImage;
use otter_vm::snapshot::IsolateSnapshot;

fn replace_raw_image(snapshot: &mut IsolateSnapshot, image: HeapImage) {
    snapshot.image = image;
}

fn main() {}
