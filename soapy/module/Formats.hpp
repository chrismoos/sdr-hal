// Host sample formats. The server's wire format is Q15, so CS16 converts exactly.

#pragma once

#include <cmath>
#include <cstddef>
#include <cstdint>
#include <string>

#include <SoapySDR/Formats.hpp>

namespace SdrHal {

static const double kCs16FullScale = 32767.0;

enum class HostFormat { Cf32, Cs16 };

inline bool parseHostFormat(const std::string &format, HostFormat &out)
{
    if (format == SOAPY_SDR_CF32) { out = HostFormat::Cf32; return true; }
    if (format == SOAPY_SDR_CS16) { out = HostFormat::Cs16; return true; }
    return false;
}

inline void cs16ToCf32(const int16_t *in, float *out, size_t samples)
{
    for (size_t i = 0; i < 2 * samples; ++i)
        out[i] = static_cast<float>(in[i] / kCs16FullScale);
}

inline void cf32ToCs16(const float *in, int16_t *out, size_t samples)
{
    for (size_t i = 0; i < 2 * samples; ++i)
    {
        double scaled = std::round(in[i] * kCs16FullScale);
        if (scaled > kCs16FullScale) scaled = kCs16FullScale;
        if (scaled < -kCs16FullScale) scaled = -kCs16FullScale;
        out[i] = static_cast<int16_t>(scaled);
    }
}

}  // namespace SdrHal
