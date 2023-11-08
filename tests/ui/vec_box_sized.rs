#![allow(dead_code)]
#![feature(allocator_api)]

struct SizedStruct(i32);
struct UnsizedStruct([i32]);
struct BigStruct([i32; 10000]);

/// The following should trigger the lint
mod should_trigger {
    use super::SizedStruct;
    const C: Vec<Box<i32>> = Vec::new();
    static S: Vec<Box<i32>> = Vec::new();

    struct StructWithVecBox {
        sized_type: Vec<Box<SizedStruct>>,
    }

    struct A(Vec<Box<SizedStruct>>);
    struct B(Vec<Vec<Box<(u32)>>>);
}

/// The following should not trigger the lint
mod should_not_trigger {
    use super::{BigStruct, UnsizedStruct};
    use std::alloc::{Layout, AllocError, Allocator};
    use std::ptr::NonNull;

    struct C(Vec<Box<UnsizedStruct>>);
    struct D(Vec<Box<BigStruct>>);

    struct StructWithVecBoxButItsUnsized {
        unsized_type: Vec<Box<UnsizedStruct>>,
    }

    struct TraitVec<T: ?Sized> {
        // Regression test for #3720. This was causing an ICE.
        inner: Vec<Box<T>>,
    }

    struct DummyAllocator;
    unsafe impl Allocator for DummyAllocator {
        fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
            todo!()
        }
        unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
            todo!()
        }
    }

    fn allocator_mismatch() -> Vec<Box<i32, DummyAllocator>> {
        vec![]
    }
}

mod inner_mod {
    mod inner {
        pub struct S;
    }

    mod inner2 {
        use super::inner::S;

        pub fn f() -> Vec<Box<S>> {
            vec![]
        }
    }
}

// https://github.com/rust-lang/rust-clippy/issues/11417
fn in_closure() {
    let _ = |_: Vec<Box<dyn ToString>>| {};
}

fn main() {}
