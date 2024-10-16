docker build -t rust-x86-cross . --file CCDockerfile && docker run --rm -v "$(pwd)":/usr/src/iroh -e CARGO_BUILD_JOBS=4 rust-x86-cross /bin/sh -c "cargo build --target x86_64-unknown-linux-gnu --release"
scp -i '../parcnet/chat/parcnet-chat.pem' target/x86_64-unknown-linux-gnu/release/iroh ubuntu@ec2-54-215-215-107.us-west-1.compute.amazonaws.com:~/
