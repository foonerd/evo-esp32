// The headers esp-idf-sys generates the `esp_audio_codec` bindings from.
// esp_audio_types.h uses uint32_t without including <stdint.h>.
#include <stdint.h>
#include <stdbool.h>
#include <stddef.h>
#include "esp_audio_types.h"
#include "esp_audio_dec.h"
#include "esp_mp3_dec.h"
