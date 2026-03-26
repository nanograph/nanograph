#ifndef NANOGRAPH_FFI_H
#define NANOGRAPH_FFI_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct NanoGraphHandle NanoGraphHandle;
typedef struct NanoGraphBytes {
    uint8_t *ptr;
    uintptr_t len;
} NanoGraphBytes;

const char *nanograph_last_error_message(void);
void nanograph_string_free(char *value);
void nanograph_bytes_free(NanoGraphBytes value);

NanoGraphHandle *nanograph_db_init(const char *db_path, const char *schema_source);
NanoGraphHandle *nanograph_db_open(const char *db_path);
NanoGraphHandle *nanograph_db_open_in_memory(const char *schema_source);
int32_t nanograph_db_close(NanoGraphHandle *handle);
void nanograph_db_destroy(NanoGraphHandle *handle);

int32_t nanograph_db_load(NanoGraphHandle *handle, const char *data_source, const char *mode);
int32_t nanograph_db_load_file(NanoGraphHandle *handle, const char *data_path, const char *mode);

char *nanograph_db_run(
    NanoGraphHandle *handle,
    const char *query_source,
    const char *query_name,
    const char *params_json
);
NanoGraphBytes nanograph_db_run_arrow(
    NanoGraphHandle *handle,
    const char *query_source,
    const char *query_name,
    const char *params_json
);
char *nanograph_arrow_to_json(const uint8_t *data, uintptr_t len);

char *nanograph_db_check(NanoGraphHandle *handle, const char *query_source);
char *nanograph_db_describe(NanoGraphHandle *handle);
char *nanograph_db_compact(NanoGraphHandle *handle, const char *options_json);
char *nanograph_db_cleanup(NanoGraphHandle *handle, const char *options_json);
char *nanograph_db_embed(NanoGraphHandle *handle, const char *options_json);
char *nanograph_db_doctor(NanoGraphHandle *handle);
int32_t nanograph_db_is_in_memory(NanoGraphHandle *handle);

#ifdef __cplusplus
}
#endif

#endif
