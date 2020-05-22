use serde::*;
use todorpc::*;

call!(  1 => Foo(u32) -> u32 {
    true
});
subs!(  2 => Bar -> (String, u32) {
    true
});
