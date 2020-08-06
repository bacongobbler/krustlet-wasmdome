# krustlet-wasmdome

Krustlet (waSCC) provider to run wasmdome mechs in Kubernetes.

Usage (make sure to edit nets-leaf.conf):

```console
$ cargo build --release
$ nats-server -c nats/nats-leaf.conf
$ ./target/release/krustlet-wasmdome
$ kubectl create -f clippy.yaml
```
