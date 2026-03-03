fn main() {
    prost_build::compile_protos(&["protobuf/webrtc.proto"], &["protobuf/"])
        .expect("Failed to compile protobuf");
}
