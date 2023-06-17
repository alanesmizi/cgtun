![cableguard logo banner](./banner.png)

# CableGuard TUN

**CableGuard TUN** is an implementation of the [WireGuard<sup>®</sup>](https://www.wireguard.com/) protocol designed for portability and speed.

The project consists of three parts:

* The executable `cableguard-cli`, a [userspace WireGuard](https://www.wireguard.com/xplatform/)
  implementation for Linux and macOS.
* The library `cableguard` that can be used to implement fast and efficient WireGuard client apps on various platforms, including iOS and Android. It implements the underlying WireGuard protocol, without the network or tunnel stacks, those can be implemented in a platform idiomatic way.
* The rodtwallet.sh scripts that work with the NEAR CLI interface. It provides barebones command line crytographic commands for the management of RODT and NEAR implicit accounts.

### Installation

You can install this project using `cargo`:

```
cargo install cableguard-cli
```

### Building

- Library only: `cargo build --lib --no-default-features --release [--target $(TARGET_TRIPLE)]`
- Executable: `cargo build --bin cableguard-cli --release [--target $(TARGET_TRIPLE)]`

By default the executable is placed in the `./target/release` folder. You can copy it to a desired location manually, or install it using `cargo install --bin cableguard --path .`.

### Running

As per the specification, to start a tunnel use:

`cableguard-cli [-f/--foreground] <filewithaccount.json>`

It may be possible to use with [wg-quick](https://git.zx2c4.com/WireGuard/about/src/tools/man/wg-quick.8) by setting the environment variable `WG_QUICK_USERSPACE_IMPLEMENTATION` to `cableguard`. For example:

`sudo WG_QUICK_USERSPACE_IMPLEMENTATION=cableguard-cli WG_SUDO=1 wg-quick up CONFIGURATION`

### Testing

Testing this project has a few requirements:

- `sudo`: required to create tunnels. When you run `cargo test` you'll be prompted for your password.
- Docker: you can install it [here](https://www.docker.com/get-started). If you are on Ubuntu/Debian you can run `apt-get install docker.io`.

## Supported platforms
It has only been tested in Linux

Target triple                 |Binary|Library|
------------------------------|:----:|------|
x86_64-unknown-linux-gnu      |  ✓   | ✓    |
aarch64-unknown-linux-gnu     |  ✓   | ✓    |
armv7-unknown-linux-gnueabihf |  ✓   | ✓    |
x86_64-apple-darwin           |  ✓   | ✓    |
x86_64-pc-windows-msvc        |      | ✓    |
aarch64-apple-ios             |      | ✓    |
armv7-apple-ios               |      | ✓    |
armv7s-apple-ios              |      | ✓    |
aarch64-linux-android         |      | ✓    |
arm-linux-androideabi         |      | ✓    |

<sub>Other platforms may be added in the future</sub>

#### Linux

`x86-64`, `aarch64` and `armv7` architectures are supported. The behaviour should be identical to that of [wireguard-go](https://git.zx2c4.com/wireguard-go/about/), with the following difference:

`cableguard` will drop privileges when started. When privileges are dropped it is not possible to set `fwmark`. If `fwmark` is required, such as when using `wg-quick`, run with `--disable-drop-privileges` or set the environment variable `WG_SUDO=1`.

You will need to give the executable the `CAP_NET_ADMIN` capability using: `sudo setcap cap_net_admin+epi cableguard`. sudo is not needed.


#### FFI bindings
- This has not been tested
The library exposes a set of C ABI bindings, those are defined in the `wireguard_ffi.h` header file. The C bindings can be used with C/C++, Swift (using a bridging header) or C# (using [DLLImport](https://docs.microsoft.com/en-us/dotnet/api/system.runtime.interopservices.dllimportattribute?view=netcore-2.2) with [CallingConvention](https://docs.microsoft.com/en-us/dotnet/api/system.runtime.interopservices.dllimportattribute.callingconvention?view=netcore-2.2) set to `Cdecl`).

#### JNI bindings
- This has not been tested
The library exposes a set of Java Native Interface bindings, those are defined in `src/jni.rs`.

## License
The project is licensed under the [3-Clause BSD License](https://opensource.org/licenses/BSD-3-Clause).

### Contribution
Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the 3-Clause BSD License, shall be licensed as above, without any additional terms or conditions.

If you want to contribute to this project, please contact <vpn@cableguard.org>.

---
<sub><sub><sub><sub>WireGuard is a registered trademark of Jason A. Donenfeld. CableGuard is not sponsored or endorsed by Jason A. Donenfeld.</sub></sub></sub></sub>