/* C interface to sdr-radiod network radios, implemented in soapy/src/ffi.rs. */

#pragma once

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Status codes. Each equals the SoapySDR error code of the same meaning. */
#define SDRHAL_OK 0
#define SDRHAL_TIMEOUT (-1)
#define SDRHAL_ERROR (-2)
#define SDRHAL_OVERFLOW (-4)
#define SDRHAL_TIME_ERROR (-6)
#define SDRHAL_UNDERFLOW (-7)

/* Directions, numbered as SOAPY_SDR_TX and SOAPY_SDR_RX. */
#define SDRHAL_TX 0
#define SDRHAL_RX 1

#define SDRHAL_TRANSPORT_UDP 0
#define SDRHAL_TRANSPORT_TCP 1

typedef struct sdrhal_device sdrhal_device;

typedef struct sdrhal_range {
    int present; /* zero when the server sets no range */
    double minimum;
    double maximum;
} sdrhal_range;

/* level runs from 1 (error) to 5 (trace). */
typedef void (*sdrhal_log_fn)(int level, const char *message);

/* Message for the last failure on the calling thread, valid until its next call. */
const char *sdrhal_last_error(void);
void sdrhal_set_log_handler(sdrhal_log_fn handler);

/* Returns NULL on failure. Zero samples_per_packet and NULL data_host take defaults. */
sdrhal_device *sdrhal_open(const char *addr, int transport, size_t samples_per_packet,
                           const char *data_host);
void sdrhal_close(sdrhal_device *device);

/* String getters copy into buf with a terminator and return the full length. */
size_t sdrhal_backend(const sdrhal_device *device, char *buf, size_t len);
size_t sdrhal_addr(const sdrhal_device *device, char *buf, size_t len);
int sdrhal_supports_rx(const sdrhal_device *device);
uint64_t sdrhal_tick_rate(const sdrhal_device *device);
size_t sdrhal_samples_per_packet(const sdrhal_device *device);
int sdrhal_ranges(const sdrhal_device *device, int direction, sdrhal_range *rate,
                  sdrhal_range *frequency);

int sdrhal_set_sample_rate(const sdrhal_device *device, int direction, double rate_hz,
                           double *applied_hz);
double sdrhal_sample_rate(const sdrhal_device *device, int direction);
int sdrhal_set_frequency(const sdrhal_device *device, int direction, double frequency_hz);
double sdrhal_frequency(const sdrhal_device *device, int direction);
int sdrhal_set_bandwidth(const sdrhal_device *device, int direction, double bandwidth_hz);
double sdrhal_bandwidth(const sdrhal_device *device, int direction);
/* present is zero when the server maintains its configured TX gain. */
int sdrhal_tx_gain_range(const sdrhal_device *device, sdrhal_range *range);
int sdrhal_set_tx_gain(const sdrhal_device *device, double gain_db, double *applied_db);
double sdrhal_tx_gain(const sdrhal_device *device);
int sdrhal_set_rx_gain(const sdrhal_device *device, double gain_db);
double sdrhal_rx_gain(const sdrhal_device *device);
int sdrhal_set_rx_antenna(const sdrhal_device *device, const char *name);
size_t sdrhal_rx_antenna(const sdrhal_device *device, char *buf, size_t len);
int sdrhal_set_tick_rate(const sdrhal_device *device, uint64_t tick_rate, uint64_t *applied);

/* Set up every stream before activating the first, since activation opens the session. */
int sdrhal_setup_stream(const sdrhal_device *device, int direction);
int sdrhal_close_stream(const sdrhal_device *device, int direction);
int sdrhal_activate(const sdrhal_device *device, int direction, int has_time,
                    long long time_ns);
int sdrhal_deactivate(const sdrhal_device *device, int direction);

/* Interleaved complex float buffers. Return a sample count or a negative status. */
long sdrhal_read(const sdrhal_device *device, float *iq, size_t samples, long timeout_us,
                 long long *time_ns);
long sdrhal_write(const sdrhal_device *device, const float *iq, size_t samples, int has_time,
                  long long time_ns, int end_of_burst, long timeout_us);
int sdrhal_tx_event(const sdrhal_device *device, long timeout_us);

int sdrhal_hardware_time(const sdrhal_device *device, long long *time_ns);
int sdrhal_set_hardware_time(const sdrhal_device *device, long long time_ns);

#ifdef __cplusplus
}
#endif
