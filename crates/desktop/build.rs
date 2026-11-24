fn main(){
    println!("cargo:rerun-if-changed=native/speech.mm");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref()==Ok("macos"){
        cc::Build::new().cpp(true).file("native/speech.mm").flag("-fobjc-arc").flag("-fblocks").compile("muteki_speech");
        for framework in ["Foundation","AVFoundation","Speech"]{println!("cargo:rustc-link-lib=framework={framework}");}
    }
}
