use todorpc::*;

#[derive(Deserialize, Serialize)]
pub struct Foo(pub u32);

impl Verify for Foo {
    fn verify(&self) -> bool {
        (self.0 & 1) == 1
    }
}

call!( Foo[1] -> u32);

#[derive(Deserialize, Serialize)]
pub struct Bar;

impl Verify for Bar {}
subs!( Bar[2] -> (String, u32) );
