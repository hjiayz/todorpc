use todorpc::*;

#[derive(Deserialize, Serialize)]
pub struct Foo(pub u32);

impl Verify for Foo {
    fn verify(&self) -> Result<(), u32> {
        if (self.0 & 1) == 1 {
            Ok(())
        } else {
            Err(0)
        }
    }
}

call!( Foo[1] -> u32 );

#[derive(Deserialize, Serialize)]
pub struct Bar;

impl Verify for Bar {}
subs!( Bar[2] -> (String, u32) );

#[derive(Deserialize, Serialize)]
pub struct UploadSample;

impl Verify for UploadSample {}
upload!( UploadSample[3](String) -> () );
