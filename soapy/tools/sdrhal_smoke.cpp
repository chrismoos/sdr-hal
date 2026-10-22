// Streams through the sdrhal SoapySDR module the way an application does and
// exits nonzero on the first failure. Usage: sdrhal_smoke "driver=sdrhal,addr=host:port"
//   [rate_hz] [tx_frequency_hz] [rx_frequency_hz] [seconds]

#include <SoapySDR/Device.hpp>
#include <SoapySDR/Errors.hpp>
#include <SoapySDR/Formats.hpp>

#include <chrono>
#include <complex>
#include <cstdio>
#include <cstdlib>
#include <stdexcept>
#include <string>
#include <vector>

namespace {

const long kTimeoutUs = 1000000;
const long long kTimedLeadNs = 50000000;

void require(bool condition, const std::string &what)
{
    if (!condition) throw std::runtime_error(what);
}

double argOr(int argc, char **argv, int index, double fallback)
{
    return argc > index ? std::atof(argv[index]) : fallback;
}

void receive(SoapySDR::Device *device, const std::string &format, double rate, double seconds)
{
    SoapySDR::Stream *stream = device->setupStream(SOAPY_SDR_RX, format);
    const size_t mtu = device->getStreamMTU(stream);
    require(device->activateStream(stream) == 0, "RX activate failed");
    std::vector<std::complex<float>> cf32(mtu);
    std::vector<std::complex<int16_t>> cs16(mtu);
    void *buffs[] = {format == SOAPY_SDR_CF32 ? static_cast<void *>(cf32.data())
                                              : static_cast<void *>(cs16.data())};
    const size_t wanted = static_cast<size_t>(rate * seconds);
    size_t received = 0;
    size_t overflows = 0;
    long long lastEndNs = -1;
    const auto started = std::chrono::steady_clock::now();
    while (received < wanted)
    {
        int flags = 0;
        long long timeNs = 0;
        const int status = device->readStream(stream, buffs, mtu, flags, timeNs, kTimeoutUs);
        if (status == SOAPY_SDR_OVERFLOW) { ++overflows; lastEndNs = -1; continue; }
        require(status > 0, std::string("RX read failed: ") + SoapySDR::errToStr(status));
        require((flags & SOAPY_SDR_HAS_TIME) != 0, "RX read returned no timestamp");
        if (lastEndNs >= 0)
            require(std::llabs(timeNs - lastEndNs) < 2000, "RX timestamps are not contiguous");
        lastEndNs = timeNs + static_cast<long long>(status * 1e9 / rate);
        received += static_cast<size_t>(status);
    }
    const double elapsed =
        std::chrono::duration<double>(std::chrono::steady_clock::now() - started).count();
    device->deactivateStream(stream);
    device->closeStream(stream);
    std::printf("RX %s: %zu samples in %.2f s (%.3f Msps), %zu overflow(s)\n", format.c_str(),
                received, elapsed, received / elapsed / 1e6, overflows);
}

void transmit(SoapySDR::Device *device, const std::string &format, double rate, double seconds)
{
    SoapySDR::Stream *stream = device->setupStream(SOAPY_SDR_TX, format);
    const size_t mtu = device->getStreamMTU(stream);
    require(device->activateStream(stream) == 0, "TX activate failed");
    std::vector<std::complex<float>> cf32(mtu, std::complex<float>(0.25f, 0.0f));
    std::vector<std::complex<int16_t>> cs16(mtu, std::complex<int16_t>(8192, 0));
    const void *buffs[] = {format == SOAPY_SDR_CF32 ? static_cast<const void *>(cf32.data())
                                                    : static_cast<const void *>(cs16.data())};

    int flags = SOAPY_SDR_HAS_TIME;
    const long long whenNs = device->getHardwareTime() + kTimedLeadNs;
    require(device->writeStream(stream, buffs, mtu, flags, whenNs, kTimeoutUs) ==
                static_cast<int>(mtu),
            "timed TX write failed");

    const size_t wanted = static_cast<size_t>(rate * seconds);
    size_t sent = 0;
    const auto started = std::chrono::steady_clock::now();
    while (sent < wanted)
    {
        flags = sent + mtu >= wanted ? SOAPY_SDR_END_BURST : 0;
        const int status = device->writeStream(stream, buffs, mtu, flags, 0, kTimeoutUs);
        if (status == SOAPY_SDR_TIMEOUT) continue;
        require(status > 0, std::string("TX write failed: ") + SoapySDR::errToStr(status));
        sent += static_cast<size_t>(status);
    }
    const double elapsed =
        std::chrono::duration<double>(std::chrono::steady_clock::now() - started).count();
    device->deactivateStream(stream);
    device->closeStream(stream);
    std::printf("TX %s: %zu untimed samples in %.2f s (%.3f Msps)\n", format.c_str(), sent,
                elapsed, sent / elapsed / 1e6);
}

}  // namespace

int main(int argc, char **argv)
{
    if (argc < 2)
    {
        std::fprintf(stderr, "usage: %s ARGS [rate_hz] [tx_hz] [rx_hz] [seconds]\n", argv[0]);
        return 2;
    }
    const double rate = argOr(argc, argv, 2, 1e6);
    const double txHz = argOr(argc, argv, 3, 915e6);
    const double rxHz = argOr(argc, argv, 4, 915e6);
    const double seconds = argOr(argc, argv, 5, 1.0);
    SoapySDR::Device *device = nullptr;
    try
    {
        device = SoapySDR::Device::make(argv[1]);
        for (const int direction : {SOAPY_SDR_TX, SOAPY_SDR_RX})
        {
            const auto rates = device->getSampleRateRange(direction, 0);
            const auto freqs = device->getFrequencyRange(direction, 0);
            std::printf("%s rates [%.0f, %.0f] Hz, frequencies [%.0f, %.0f] Hz\n",
                        direction == SOAPY_SDR_TX ? "TX" : "RX", rates.front().minimum(),
                        rates.back().maximum(), freqs.front().minimum(), freqs.back().maximum());
        }
        // A single-client server keeps its first session's rates, so every session
        // here uses the same ones.
        device->setSampleRate(SOAPY_SDR_RX, 0, rate);
        device->setFrequency(SOAPY_SDR_RX, 0, rxHz);
        device->setSampleRate(SOAPY_SDR_TX, 0, rate);
        device->setFrequency(SOAPY_SDR_TX, 0, txHz);
        receive(device, SOAPY_SDR_CF32, rate, seconds);
        receive(device, SOAPY_SDR_CS16, rate, seconds);
        transmit(device, SOAPY_SDR_CF32, rate, seconds);
        transmit(device, SOAPY_SDR_CS16, rate, seconds);
        SoapySDR::Device::unmake(device);
    }
    catch (const std::exception &error)
    {
        std::fprintf(stderr, "sdrhal_smoke: %s\n", error.what());
        if (device != nullptr) SoapySDR::Device::unmake(device);
        return 1;
    }
    std::printf("ok\n");
    return 0;
}
