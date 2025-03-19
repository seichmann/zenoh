for pid in $(ps -aux | grep /zenohd | awk '{print $2}'); do kill $pid; done;

cargo build --bin zenohd
cp target/debug/zenohd ../../zenoh-1.2.1-own-build/

#cargo build --release --all-targets
#cp target/release/zenohd ../../zenoh-1.2.1-own-build/
