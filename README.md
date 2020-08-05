# krustlet-wasmdome

Krustlet (waSCC) provider to run wasmdome mechs in Kubernetes.

Usage (make sure to edit nets-leaf.conf):

```console
$ cargo build --release
$ docker run -d -v `pwd`/nats:/etc/nats -p 4222:4222 -p 6222:6222 -p 8222:8222 nats -c /etc/nats/nats-leaf.conf
$ ./target/release/krustlet-wasmdome
$ kubectl create -f clippy.yaml
```
