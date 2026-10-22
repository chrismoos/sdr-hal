#include "SdrHal.hpp"
#include "Formats.hpp"

#include <SoapySDR/Constants.h>
#include <SoapySDR/Errors.hpp>
#include <SoapySDR/Logger.hpp>

#include <cstdlib>
#include <stdexcept>

namespace SdrHal {

namespace {

struct Stream {
    int direction = SOAPY_SDR_RX;
    HostFormat format = HostFormat::Cf32;
    std::vector<float> scratch;
};

std::string lastError()
{
    const char *message = sdrhal_last_error();
    return message != nullptr ? message : "unknown error";
}

[[noreturn]] void fail(const std::string &context)
{
    throw std::runtime_error("sdrhal: " + context + ": " + lastError());
}

void check(int status, const std::string &context)
{
    if (status != SDRHAL_OK) fail(context);
}

// Stream calls report failure as a status code rather than an exception, so a
// generic error is logged with its message before it is returned.
int streamStatus(long status, const char *context)
{
    if (status == SDRHAL_ERROR)
        SoapySDR::logf(SOAPY_SDR_ERROR, "sdrhal: %s: %s", context, lastError().c_str());
    return static_cast<int>(status);
}

std::string copyString(size_t (*getter)(const sdrhal_device *, char *, size_t),
                       const sdrhal_device *device)
{
    const size_t length = getter(device, nullptr, 0);
    std::string text(length + 1, '\0');
    getter(device, &text[0], text.size());
    text.resize(length);
    return text;
}

Stream *asStream(SoapySDR::Stream *stream)
{
    return reinterpret_cast<Stream *>(stream);
}

std::string argOr(const SoapySDR::Kwargs &args, const char *key, const std::string &fallback)
{
    const auto found = args.find(key);
    return found != args.end() ? found->second : fallback;
}

}  // namespace

std::string controlAddress(const std::string &addr)
{
    if (addr.empty()) throw std::invalid_argument("sdrhal: addr is required");
    const bool bracketed = addr.front() == '[';
    const size_t close = addr.find(']');
    const bool hasPort = bracketed ? (close != std::string::npos && close + 1 < addr.size() &&
                                      addr[close + 1] == ':')
                                   : addr.find(':') != std::string::npos;
    return hasPort ? addr : addr + ":" + kDefaultControlPort;
}

Device::Device(const SoapySDR::Kwargs &args)
{
    addr_ = controlAddress(argOr(args, "addr", ""));
    const std::string transport = argOr(args, "transport", "tcp");
    int transportCode = SDRHAL_TRANSPORT_TCP;
    if (transport == "udp") transportCode = SDRHAL_TRANSPORT_UDP;
    else if (transport != "tcp")
        throw std::invalid_argument("sdrhal: transport must be udp or tcp, not " + transport);
    const size_t samplesPerPacket =
        static_cast<size_t>(std::strtoul(argOr(args, "samples_per_packet", "0").c_str(), nullptr, 10));
    const auto dataHost = args.find("data_host");

    device_ = sdrhal_open(addr_.c_str(), transportCode, samplesPerPacket,
                          dataHost != args.end() ? dataHost->second.c_str() : nullptr);
    if (device_ == nullptr) fail("cannot open " + addr_);
    backend_ = copyString(sdrhal_backend, device_);
    hardwareKey_ = argOr(args, "hardware", backend_);
    SoapySDR::logf(SOAPY_SDR_DEBUG, "sdrhal: connected to %s, backend %s, %s transport",
                   addr_.c_str(), backend_.c_str(), transport.c_str());
}

Device::~Device()
{
    sdrhal_close(device_);
}

std::string Device::getDriverKey(void) const { return kDriverKey; }

std::string Device::getHardwareKey(void) const { return hardwareKey_; }

SoapySDR::Kwargs Device::getHardwareInfo(void) const
{
    SoapySDR::Kwargs info;
    info["addr"] = addr_;
    info["backend"] = backend_;
    info["tick_rate"] = std::to_string(sdrhal_tick_rate(device_));
    return info;
}

size_t Device::getNumChannels(const int direction) const
{
    if (direction == SOAPY_SDR_RX) return sdrhal_supports_rx(device_) ? 1 : 0;
    return 1;
}

bool Device::getFullDuplex(const int, const size_t) const { return true; }

void Device::checkChannel(const int direction, const size_t channel) const
{
    if (channel >= getNumChannels(direction))
        throw std::out_of_range("sdrhal: no channel " + std::to_string(channel));
}

std::vector<std::string> Device::getStreamFormats(const int, const size_t) const
{
    return {SOAPY_SDR_CF32, SOAPY_SDR_CS16};
}

std::string Device::getNativeStreamFormat(const int, const size_t, double &fullScale) const
{
    fullScale = kCs16FullScale;
    return SOAPY_SDR_CS16;
}

SoapySDR::Stream *Device::setupStream(const int direction, const std::string &format,
                                      const std::vector<size_t> &channels,
                                      const SoapySDR::Kwargs &)
{
    if (channels.size() > 1 || (channels.size() == 1 && channels[0] != 0))
        throw std::invalid_argument("sdrhal: only channel 0 can stream");
    auto stream = new Stream();
    stream->direction = direction;
    if (!parseHostFormat(format, stream->format))
    {
        delete stream;
        throw std::invalid_argument("sdrhal: unsupported stream format " + format);
    }
    if (sdrhal_setup_stream(device_, direction) != SDRHAL_OK)
    {
        delete stream;
        fail("setupStream");
    }
    return reinterpret_cast<SoapySDR::Stream *>(stream);
}

void Device::closeStream(SoapySDR::Stream *handle)
{
    Stream *stream = asStream(handle);
    if (sdrhal_close_stream(device_, stream->direction) != SDRHAL_OK)
        SoapySDR::logf(SOAPY_SDR_WARNING, "sdrhal: closeStream: %s", lastError().c_str());
    delete stream;
}

size_t Device::getStreamMTU(SoapySDR::Stream *) const
{
    return sdrhal_samples_per_packet(device_);
}

int Device::activateStream(SoapySDR::Stream *handle, const int flags, const long long timeNs,
                           const size_t numElems)
{
    if ((flags & SOAPY_SDR_END_BURST) != 0 && numElems != 0) return SOAPY_SDR_NOT_SUPPORTED;
    const int hasTime = (flags & SOAPY_SDR_HAS_TIME) != 0;
    return streamStatus(sdrhal_activate(device_, asStream(handle)->direction, hasTime, timeNs),
                        "activateStream");
}

int Device::deactivateStream(SoapySDR::Stream *handle, const int, const long long)
{
    return streamStatus(sdrhal_deactivate(device_, asStream(handle)->direction),
                        "deactivateStream");
}

int Device::readStream(SoapySDR::Stream *handle, void *const *buffs, const size_t numElems,
                       int &flags, long long &timeNs, const long timeoutUs)
{
    Stream *stream = asStream(handle);
    flags = 0;
    float *out = static_cast<float *>(buffs[0]);
    if (stream->format == HostFormat::Cs16)
    {
        stream->scratch.resize(2 * numElems);
        out = stream->scratch.data();
    }
    long long firstNs = 0;
    const long status = sdrhal_read(device_, out, numElems, timeoutUs, &firstNs);
    if (status < 0) return streamStatus(status, "readStream");
    if (stream->format == HostFormat::Cs16)
        cf32ToCs16(out, static_cast<int16_t *>(buffs[0]), static_cast<size_t>(status));
    flags = SOAPY_SDR_HAS_TIME;
    timeNs = firstNs;
    return static_cast<int>(status);
}

int Device::writeStream(SoapySDR::Stream *handle, const void *const *buffs, const size_t numElems,
                        int &flags, const long long timeNs, const long timeoutUs)
{
    Stream *stream = asStream(handle);
    const float *in = static_cast<const float *>(buffs[0]);
    if (stream->format == HostFormat::Cs16)
    {
        stream->scratch.resize(2 * numElems);
        cs16ToCf32(static_cast<const int16_t *>(buffs[0]), stream->scratch.data(), numElems);
        in = stream->scratch.data();
    }
    const int hasTime = (flags & SOAPY_SDR_HAS_TIME) != 0;
    const int endOfBurst = (flags & SOAPY_SDR_END_BURST) != 0;
    flags = 0;
    return streamStatus(
        sdrhal_write(device_, in, numElems, hasTime, timeNs, endOfBurst, timeoutUs),
        "writeStream");
}

int Device::readStreamStatus(SoapySDR::Stream *handle, size_t &chanMask, int &flags,
                             long long &timeNs, const long timeoutUs)
{
    if (asStream(handle)->direction != SOAPY_SDR_TX) return SOAPY_SDR_NOT_SUPPORTED;
    flags = 0;
    timeNs = 0;
    const int status = streamStatus(sdrhal_tx_event(device_, timeoutUs), "readStreamStatus");
    if (status == SDRHAL_UNDERFLOW || status == SDRHAL_TIME_ERROR) chanMask = 1;
    return status;
}

std::vector<std::string> Device::listAntennas(const int direction, const size_t channel) const
{
    checkChannel(direction, channel);
    std::vector<std::string> names{kDefaultAntenna};
    const std::string current = getAntenna(direction, channel);
    if (current != kDefaultAntenna) names.push_back(current);
    return names;
}

void Device::setAntenna(const int direction, const size_t channel, const std::string &name)
{
    checkChannel(direction, channel);
    if (direction == SOAPY_SDR_TX)
    {
        if (name != kDefaultAntenna)
            throw std::invalid_argument("sdrhal: the TX port is set in the server's radio configuration");
        return;
    }
    check(sdrhal_set_rx_antenna(device_, name == kDefaultAntenna ? "" : name.c_str()),
          "setAntenna");
}

std::string Device::getAntenna(const int direction, const size_t channel) const
{
    checkChannel(direction, channel);
    if (direction == SOAPY_SDR_TX) return kDefaultAntenna;
    const std::string name = copyString(sdrhal_rx_antenna, device_);
    return name.empty() ? kDefaultAntenna : name;
}

bool Device::serverTxGain(sdrhal_range &range) const
{
    range = sdrhal_range{};
    check(sdrhal_tx_gain_range(device_, &range), "TX gain range");
    return range.present != 0;
}

std::vector<std::string> Device::listGains(const int direction, const size_t channel) const
{
    checkChannel(direction, channel);
    sdrhal_range range{};
    if (direction == SOAPY_SDR_TX && !serverTxGain(range)) return {};
    return {kGainElement};
}

void Device::setGain(const int direction, const size_t channel, const double value)
{
    checkChannel(direction, channel);
    sdrhal_range range{};
    if (direction == SOAPY_SDR_TX && serverTxGain(range))
    {
        double applied = 0.0;
        check(sdrhal_set_tx_gain(device_, value, &applied), "setGain");
        return;
    }
    if (direction == SOAPY_SDR_TX)
    {
        // A composite server owns TX gain. Accepting it keeps apps that always set one working.
        txGainDb_ = value;
        if (!warnedTxGain_.exchange(true))
            SoapySDR::log(SOAPY_SDR_WARNING,
                          "sdrhal: TX gain is set in the server's radio configuration, ignoring it here");
        return;
    }
    check(sdrhal_set_rx_gain(device_, value), "setGain");
}

double Device::getGain(const int direction, const size_t channel) const
{
    checkChannel(direction, channel);
    if (direction != SOAPY_SDR_TX) return sdrhal_rx_gain(device_);
    sdrhal_range range{};
    return serverTxGain(range) ? sdrhal_tx_gain(device_) : txGainDb_;
}

SoapySDR::Range Device::getGainRange(const int direction, const size_t channel) const
{
    checkChannel(direction, channel);
    if (direction == SOAPY_SDR_TX)
    {
        sdrhal_range range{};
        if (!serverTxGain(range)) return SoapySDR::Range(0.0, 0.0);
        return SoapySDR::Range(range.minimum, range.maximum);
    }
    return SoapySDR::Range(0.0, kFallbackMaxGainDb);
}

void Device::setGain(const int direction, const size_t channel, const std::string &,
                     const double value)
{
    setGain(direction, channel, value);
}

double Device::getGain(const int direction, const size_t channel, const std::string &) const
{
    return getGain(direction, channel);
}

SoapySDR::Range Device::getGainRange(const int direction, const size_t channel,
                                     const std::string &) const
{
    return getGainRange(direction, channel);
}

void Device::setFrequency(const int direction, const size_t channel, const double frequency,
                          const SoapySDR::Kwargs &)
{
    checkChannel(direction, channel);
    check(sdrhal_set_frequency(device_, direction, frequency), "setFrequency");
}

double Device::getFrequency(const int direction, const size_t channel) const
{
    checkChannel(direction, channel);
    return sdrhal_frequency(device_, direction);
}

void Device::setFrequency(const int direction, const size_t channel, const std::string &,
                          const double frequency, const SoapySDR::Kwargs &args)
{
    setFrequency(direction, channel, frequency, args);
}

double Device::getFrequency(const int direction, const size_t channel,
                            const std::string &) const
{
    return getFrequency(direction, channel);
}

std::vector<std::string> Device::listFrequencies(const int, const size_t) const
{
    return {kFrequencyComponent};
}

sdrhal_range Device::range(const int direction, bool frequency) const
{
    sdrhal_range rate{};
    sdrhal_range carrier{};
    check(sdrhal_ranges(device_, direction, &rate, &carrier), "ranges");
    return frequency ? carrier : rate;
}

SoapySDR::RangeList Device::getFrequencyRange(const int direction, const size_t channel) const
{
    checkChannel(direction, channel);
    const sdrhal_range found = range(direction, true);
    if (!found.present) return {SoapySDR::Range(kFallbackMinFrequencyHz, kFallbackMaxFrequencyHz)};
    return {SoapySDR::Range(found.minimum, found.maximum)};
}

SoapySDR::RangeList Device::getFrequencyRange(const int direction, const size_t channel,
                                              const std::string &) const
{
    return getFrequencyRange(direction, channel);
}

void Device::setSampleRate(const int direction, const size_t channel, const double rate)
{
    checkChannel(direction, channel);
    double applied = 0.0;
    check(sdrhal_set_sample_rate(device_, direction, rate, &applied), "setSampleRate");
    if (applied != rate)
        SoapySDR::logf(SOAPY_SDR_INFO, "sdrhal: %s rate %.3f Hz applied as %.3f Hz",
                       direction == SOAPY_SDR_TX ? "TX" : "RX", rate, applied);
}

double Device::getSampleRate(const int direction, const size_t channel) const
{
    checkChannel(direction, channel);
    return sdrhal_sample_rate(device_, direction);
}

SoapySDR::RangeList Device::getSampleRateRange(const int direction, const size_t channel) const
{
    checkChannel(direction, channel);
    const sdrhal_range found = range(direction, false);
    if (!found.present) return {SoapySDR::Range(kFallbackMinRateHz, kFallbackMaxRateHz)};
    return {SoapySDR::Range(found.minimum, found.maximum)};
}

void Device::setBandwidth(const int direction, const size_t channel, const double bw)
{
    checkChannel(direction, channel);
    check(sdrhal_set_bandwidth(device_, direction, bw), "setBandwidth");
}

double Device::getBandwidth(const int direction, const size_t channel) const
{
    checkChannel(direction, channel);
    return sdrhal_bandwidth(device_, direction);
}

void Device::setMasterClockRate(const double rate)
{
    if (!(rate >= 1.0)) throw std::invalid_argument("sdrhal: clock rate must be at least 1 Hz");
    uint64_t applied = 0;
    check(sdrhal_set_tick_rate(device_, static_cast<uint64_t>(rate + 0.5), &applied),
          "setMasterClockRate");
}

double Device::getMasterClockRate(void) const
{
    return static_cast<double>(sdrhal_tick_rate(device_));
}

bool Device::hasHardwareTime(const std::string &what) const { return what.empty(); }

long long Device::getHardwareTime(const std::string &what) const
{
    if (!what.empty()) throw std::invalid_argument("sdrhal: no time source named " + what);
    long long now = 0;
    check(sdrhal_hardware_time(device_, &now), "getHardwareTime");
    return now;
}

void Device::setHardwareTime(const long long timeNs, const std::string &what)
{
    if (!what.empty()) throw std::invalid_argument("sdrhal: no time source named " + what);
    check(sdrhal_set_hardware_time(device_, timeNs), "setHardwareTime");
}

}  // namespace SdrHal
