
### install certgen
```
cargo install --git https://github.com/hjiayz/certgen.git --force
```

### generate new p12 cert for native-tls
```
certgen
```

### run server
```
cargo run --release
```