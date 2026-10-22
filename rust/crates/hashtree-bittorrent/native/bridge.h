#pragma once
#include <stddef.h>
#include <stdint.h>

extern "C" {
struct ht_file { char const* path; uint64_t size; };
struct ht_options {
    char const* listen;
    char const* bootstrap;
    uint64_t cache_bytes;
    uint32_t connections, upload_limit, download_limit;
    uint8_t dht, trackers;
};
struct ht_stats {
    uint64_t downloaded, uploaded, cache_bytes;
    uint32_t peers, pieces;
    uint16_t port;
};
typedef int (*ht_read_fn)(void*, uint32_t, uint64_t, uint32_t, char*);
typedef int (*ht_cancel_fn)(void*);
char const* ht_error();
int ht_error_kind();
void* ht_meta_parse(char const*, size_t);
void* ht_meta_create(ht_file const*, size_t, int, int, ht_read_fn, void*, ht_cancel_fn, void*);
void ht_meta_free(void*);
size_t ht_meta_size(void const*);
char const* ht_meta_bytes(void const*);
int ht_meta_file_count(void const*);
char const* ht_meta_file_path(void const*, int);
uint64_t ht_meta_file_size(void const*, int);
int ht_meta_file_flags(void const*, int);
int ht_meta_piece_length(void const*);
char const* ht_meta_magnet(void const*);
void* ht_session_open(void const*, ht_options const*, ht_file const*, size_t, ht_read_fn, void*);
void* ht_session_magnet(char const*, ht_options const*);
void* ht_session_metadata(void*, uint64_t, ht_cancel_fn, void*);
void ht_session_free(void*);
int ht_session_connect(void*, char const*, uint16_t);
int ht_session_read(void*, int, uint64_t, uint32_t, char*, uint64_t, ht_cancel_fn, void*);
int ht_session_stats(void*, ht_stats*);
}
