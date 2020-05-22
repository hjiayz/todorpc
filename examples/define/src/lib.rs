use serde::*;
use todorpc::*;

call!(  1 => Foo(src:u32) -> u32 {
    src<1000000
});
subs!(  2 => Bar -> (String, u32) {
    true
});
