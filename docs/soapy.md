# SoapySDR module

The `soapy/` module lets GNU Radio, SDR++, SDRangel and other SoapySDR
applications use an `sdr-radiod` server as a local device.

## Building

Building the module requires SoapySDR's development files, CMake and Cargo:

```sh
cmake -S soapy -B build/soapy && cmake --build build/soapy
sudo cmake --install build/soapy
```

On macOS with Homebrew, the module must be installed into Homebrew's prefix for
its SoapySDR to find it:

```sh
brew install soapysdr cmake
cmake -S soapy -B build/soapy && cmake --build build/soapy
cmake --install build/soapy --prefix "$(brew --prefix)"
```

## Opening a device

The device is opened by address:

```sh
SoapySDRUtil --probe="driver=sdrhal,addr=radio-host:50710"
```

| Argument | Meaning |
| --- | --- |
| `addr` | server address. Port 50710 when omitted |
| `transport` | `tcp` (default) or `udp` |
| `hardware` | board name the driver reports, such as `B210` for applications that check it. The server's backend when omitted |

## Server modes

| | Single-client server | Composite server |
| --- | --- | --- |
| Rates and frequencies offered | any the device supports | only those the composite plan accepts |
| TX gain | set by the application | fixed by the server |
| Sample rate after the first stream | kept until the server restarts | can change between streams |

Stream settings are locked while streaming. The stream must be stopped to
change the sample rate.
