#pragma once
#include "bridge.h"
#include <libtorrent/add_torrent_params.hpp>
#include <libtorrent/disk_interface.hpp>
#include <libtorrent/file_storage.hpp>
#include <libtorrent/session_params.hpp>
#include <libtorrent/torrent_info.hpp>
#include <chrono>
#include <map>
#include <memory>
#include <mutex>
#include <set>
#include <stdexcept>
#include <string>
#include <vector>

namespace ht {
namespace lt = libtorrent;
constexpr std::size_t max_metadata = 16 * 1024 * 1024;
constexpr int max_piece = 16 * 1024 * 1024;
constexpr int max_files = 100000;
constexpr int max_pieces = 1000000;
struct failure : std::runtime_error {
    int kind;
    explicit failure(std::string s, int k = 0): std::runtime_error(std::move(s)), kind(k) {}
};
void save_error() noexcept;
void clear_error() noexcept;
struct metadata {
    lt::add_torrent_params params;
    std::vector<char> bytes;
    std::vector<std::string> paths;
    std::string magnet;
    explicit metadata(lt::add_torrent_params p, std::vector<char> b = {});
};
struct source {
    std::map<std::string, std::uint32_t> files;
    ht_read_fn read = nullptr;
    void* context = nullptr;
    ht_cancel_fn cancelled = nullptr;
    void* cancel_context = nullptr;
};
struct piece {
    std::vector<char> data;
    bool verified = false;
    std::uint64_t age = 0;
};
struct cache {
    std::mutex mutex;
    std::map<int, piece> pieces;
    std::set<int> wanted;
    std::size_t bytes = 0, limit = 0;
    std::uint64_t clock = 0;
};
lt::disk_io_constructor_type storage_factory(std::shared_ptr<source>, std::shared_ptr<cache>);
std::shared_ptr<source> make_source(ht_file const*, std::size_t, ht_read_fn, void*);
lt::settings_pack settings(ht_options const&, bool seed);
void validate(lt::torrent_info const&);
}
