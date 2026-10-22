// SoapySDR device for sdr-radiod network radios. The Rust library behind
// sdrhal_soapy.h talks to the server, and this class adapts it to SoapySDR.

#pragma once

#include <sdrhal_soapy.h>

#include <SoapySDR/Device.hpp>
#include <SoapySDR/Types.hpp>

#include <atomic>
#include <string>
#include <vector>

namespace SdrHal {

static const char *const kDriverKey = "sdrhal";
static const char *const kDefaultControlPort = "50710";
static const char *const kDefaultAntenna = "DEFAULT";
static const char *const kGainElement = "GAIN";
static const char *const kFrequencyComponent = "RF";

// Ranges for a server that reports none of its own. They only bound what an
// application offers, and the server still refuses what its device cannot do.
static const double kFallbackMinRateHz = 1e3;
static const double kFallbackMaxRateHz = 200e6;
static const double kFallbackMinFrequencyHz = 0.0;
static const double kFallbackMaxFrequencyHz = 6e9;
// The protocol has no gain range, so this is a wide bound rather than the device's.
static const double kFallbackMaxGainDb = 100.0;

std::string controlAddress(const std::string &addr);

class Device : public SoapySDR::Device {
public:
    explicit Device(const SoapySDR::Kwargs &args);
    ~Device() override;

    std::string getDriverKey(void) const override;
    std::string getHardwareKey(void) const override;
    SoapySDR::Kwargs getHardwareInfo(void) const override;

    size_t getNumChannels(const int direction) const override;
    bool getFullDuplex(const int direction, const size_t channel) const override;

    std::vector<std::string> getStreamFormats(const int direction,
                                              const size_t channel) const override;
    std::string getNativeStreamFormat(const int direction, const size_t channel,
                                      double &fullScale) const override;

    SoapySDR::Stream *setupStream(const int direction, const std::string &format,
                                  const std::vector<size_t> &channels,
                                  const SoapySDR::Kwargs &args) override;
    void closeStream(SoapySDR::Stream *stream) override;
    size_t getStreamMTU(SoapySDR::Stream *stream) const override;
    int activateStream(SoapySDR::Stream *stream, const int flags, const long long timeNs,
                       const size_t numElems) override;
    int deactivateStream(SoapySDR::Stream *stream, const int flags,
                         const long long timeNs) override;
    int readStream(SoapySDR::Stream *stream, void *const *buffs, const size_t numElems, int &flags,
                   long long &timeNs, const long timeoutUs) override;
    int writeStream(SoapySDR::Stream *stream, const void *const *buffs, const size_t numElems,
                    int &flags, const long long timeNs, const long timeoutUs) override;
    int readStreamStatus(SoapySDR::Stream *stream, size_t &chanMask, int &flags, long long &timeNs,
                         const long timeoutUs) override;

    std::vector<std::string> listAntennas(const int direction,
                                          const size_t channel) const override;
    void setAntenna(const int direction, const size_t channel, const std::string &name) override;
    std::string getAntenna(const int direction, const size_t channel) const override;

    std::vector<std::string> listGains(const int direction, const size_t channel) const override;
    void setGain(const int direction, const size_t channel, const double value) override;
    double getGain(const int direction, const size_t channel) const override;
    SoapySDR::Range getGainRange(const int direction, const size_t channel) const override;
    void setGain(const int direction, const size_t channel, const std::string &name,
                 const double value) override;
    double getGain(const int direction, const size_t channel,
                   const std::string &name) const override;
    SoapySDR::Range getGainRange(const int direction, const size_t channel,
                                 const std::string &name) const override;

    void setFrequency(const int direction, const size_t channel, const double frequency,
                      const SoapySDR::Kwargs &args) override;
    double getFrequency(const int direction, const size_t channel) const override;
    void setFrequency(const int direction, const size_t channel, const std::string &name,
                      const double frequency, const SoapySDR::Kwargs &args) override;
    double getFrequency(const int direction, const size_t channel,
                        const std::string &name) const override;
    std::vector<std::string> listFrequencies(const int direction,
                                             const size_t channel) const override;
    SoapySDR::RangeList getFrequencyRange(const int direction,
                                          const size_t channel) const override;
    SoapySDR::RangeList getFrequencyRange(const int direction, const size_t channel,
                                          const std::string &name) const override;

    void setSampleRate(const int direction, const size_t channel, const double rate) override;
    double getSampleRate(const int direction, const size_t channel) const override;
    SoapySDR::RangeList getSampleRateRange(const int direction,
                                           const size_t channel) const override;

    void setBandwidth(const int direction, const size_t channel, const double bw) override;
    double getBandwidth(const int direction, const size_t channel) const override;

    void setMasterClockRate(const double rate) override;
    double getMasterClockRate(void) const override;

    bool hasHardwareTime(const std::string &what) const override;
    long long getHardwareTime(const std::string &what) const override;
    void setHardwareTime(const long long timeNs, const std::string &what) override;

private:
    void checkChannel(const int direction, const size_t channel) const;
    sdrhal_range range(const int direction, bool frequency) const;

    sdrhal_device *device_ = nullptr;
    std::string backend_;
    std::string hardwareKey_;
    std::string addr_;
    bool serverTxGain(sdrhal_range &range) const;
    double txGainDb_ = 0.0;
    std::atomic<bool> warnedTxGain_{false};
};

}  // namespace SdrHal
