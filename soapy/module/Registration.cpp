#include "SdrHal.hpp"

#include <SoapySDR/Logger.hpp>
#include <SoapySDR/Registry.hpp>

#include <exception>

namespace SdrHal {

namespace {

void forwardLog(int level, const char *message)
{
    SoapySDR::LogLevel soapyLevel = SOAPY_SDR_TRACE;
    switch (level)
    {
    case 1: soapyLevel = SOAPY_SDR_ERROR; break;
    case 2: soapyLevel = SOAPY_SDR_WARNING; break;
    case 3: soapyLevel = SOAPY_SDR_INFO; break;
    case 4: soapyLevel = SOAPY_SDR_DEBUG; break;
    default: break;
    }
    SoapySDR::log(soapyLevel, message);
}

void installLogging()
{
    static const bool installed = (sdrhal_set_log_handler(&forwardLog), true);
    (void)installed;
}

// The server is not discoverable, so a device is only found at an explicit addr.
SoapySDR::KwargsList find(const SoapySDR::Kwargs &args)
{
    installLogging();
    SoapySDR::KwargsList results;
    const auto driver = args.find("driver");
    if (driver != args.end() && driver->second != kDriverKey) return results;
    const auto addr = args.find("addr");
    if (addr == args.end()) return results;
    try
    {
        Device probe(args);
        SoapySDR::Kwargs found = args;
        found["driver"] = kDriverKey;
        found["addr"] = controlAddress(addr->second);
        found["backend"] = probe.getHardwareKey();
        found["label"] = "sdr-radiod " + probe.getHardwareKey() + " at " + found["addr"];
        results.push_back(found);
    }
    catch (const std::exception &error)
    {
        SoapySDR::logf(SOAPY_SDR_DEBUG, "sdrhal: no server at %s: %s", addr->second.c_str(),
                       error.what());
    }
    return results;
}

SoapySDR::Device *make(const SoapySDR::Kwargs &args)
{
    installLogging();
    return new Device(args);
}

SoapySDR::Registry registration(kDriverKey, &find, &make, SOAPY_SDR_ABI_VERSION);

}  // namespace

}  // namespace SdrHal
