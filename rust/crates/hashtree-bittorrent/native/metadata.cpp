#include "internal.hpp"
#include <libtorrent/create_torrent.hpp>
#include <libtorrent/load_torrent.hpp>
#include <libtorrent/magnet_uri.hpp>
#include <libtorrent/settings_pack.hpp>
#include <libtorrent/write_resume_data.hpp>
#include <cstring>
#include <limits>

namespace ht {
namespace {
thread_local std::string error_message;
thread_local int error_kind = 0;
}
void clear_error() noexcept { error_message.clear(); error_kind = 0; }
void save_error() noexcept {
    try { throw; }
    catch (failure const& e) { error_message = e.what(); error_kind = e.kind; }
    catch (std::exception const& e) { error_message = e.what(); error_kind = 0; }
    catch (...) { error_message = "unknown native exception"; error_kind = 0; }
}
void validate(lt::torrent_info const& ti) {
    if (ti.num_files() <= 0 || ti.num_files() > max_files
        || ti.num_pieces() <= 0 || ti.num_pieces() > max_pieces
        || ti.piece_length() < 16384 || ti.piece_length() > max_piece)
        throw failure("torrent exceeds file, piece-count or piece-size limits");
    std::set<std::string> paths;
    auto const& fs = ti.layout();
    for (auto f : fs.file_range()) {
        if (fs.pad_file_at(f)) continue;
        if (fs.file_flags(f) & lt::file_storage::flag_symlink)
            throw failure("symlink torrents are not supported");
        auto const path = fs.file_path(f);
        if (path.empty() || path.size() > 4096 || !paths.insert(path).second)
            throw failure("invalid or duplicate torrent path");
    }
}
metadata::metadata(lt::add_torrent_params p, std::vector<char> b)
    : params(std::move(p)), bytes(std::move(b)) {
    if (!params.ti) throw failure("torrent has no metadata");
    validate(*params.ti);
    for (auto f : params.ti->layout().file_range()) paths.push_back(params.ti->layout().file_path(f));
    // Only exact topics and the public display name are emitted, never local peers or paths.
    lt::add_torrent_params clean;
    clean.ti = params.ti;
    clean.info_hashes = params.ti->info_hashes();
    clean.name = params.ti->name();
    magnet = lt::make_magnet_uri(clean);
}
std::shared_ptr<source> make_source(ht_file const* files, std::size_t count, ht_read_fn read, void* ctx) {
    auto src = std::make_shared<source>();
    src->read = read;
    src->context = ctx;
    for (std::size_t i = 0; i < count; ++i)
        if (!src->files.emplace(files[i].path, std::uint32_t(i)).second)
            throw failure("duplicate snapshot file");
    return src;
}
}

extern "C" {
char const* ht_error() { return ht::error_message.c_str(); }
int ht_error_kind() { return ht::error_kind; }
void* ht_meta_parse(char const* data, size_t size) {
    ht::clear_error();
    try {
        if (!data || size == 0 || size > ht::max_metadata) throw ht::failure("invalid metadata size");
        libtorrent::load_torrent_limits limits;
        limits.max_buffer_size = int(ht::max_metadata);
        limits.max_pieces = ht::max_pieces;
        auto params = libtorrent::load_torrent_buffer({data, std::ptrdiff_t(size)}, limits);
        return new ht::metadata(std::move(params), std::vector<char>(data, data + size));
    } catch (...) { ht::save_error(); return nullptr; }
}
void* ht_meta_create(ht_file const* files, size_t count, int format, int piece_size,
    ht_read_fn read, void* ctx, ht_cancel_fn cancelled, void* cancel_ctx) {
    ht::clear_error();
    try {
        if (!read || count == 0 || count > ht::max_files || format < 0 || format > 2
            || piece_size < 16384 || piece_size > ht::max_piece || (piece_size & (piece_size - 1)))
            throw ht::failure("invalid torrent creation options");
        std::vector<libtorrent::create_file_entry> entries;
        std::uint64_t total = 0, padded_pieces = 0;
        for (size_t i = 0; i < count; ++i) {
            if (files[i].size > std::uint64_t(std::numeric_limits<std::int64_t>::max()) - total)
                throw ht::failure("file is too large");
            total += files[i].size;
            padded_pieces += (files[i].size + std::uint64_t(piece_size) - 1) / std::uint64_t(piece_size);
            entries.emplace_back(files[i].path, std::int64_t(files[i].size));
        }
        auto const pieces = format == 1 ? (total + std::uint64_t(piece_size) - 1) / std::uint64_t(piece_size) : padded_pieces;
        if (pieces == 0 || pieces > ht::max_pieces)
            throw ht::failure("snapshot is empty or exceeds the piece-count limit");
        auto flags = format == 1 ? libtorrent::create_torrent::v1_only
            : format == 2 ? libtorrent::create_torrent::v2_only : libtorrent::create_flags_t{};
        libtorrent::create_torrent creator(std::move(entries), piece_size, flags);
        creator.set_creation_date(0);
        auto src = ht::make_source(files, count, read, ctx);
        src->cancelled = cancelled;
        src->cancel_context = cancel_ctx;
        libtorrent::settings_pack opts;
        opts.set_int(libtorrent::settings_pack::aio_threads, 2);
        libtorrent::error_code ec;
        libtorrent::set_piece_hashes(creator, ".", opts, ht::storage_factory(src, nullptr),
            [](libtorrent::piece_index_t) {}, ec);
        if (cancelled && cancelled(cancel_ctx)) throw ht::failure("operation cancelled", 2);
        if (ec) throw ht::failure("snapshot hashing failed: " + ec.message());
        auto bytes = creator.generate_buf();
        if (bytes.size() > ht::max_metadata) throw ht::failure("generated metadata exceeds size limit");
        auto params = libtorrent::load_torrent_buffer(bytes);
        return new ht::metadata(std::move(params), std::move(bytes));
    } catch (...) { ht::save_error(); return nullptr; }
}
void ht_meta_free(void* p) { delete static_cast<ht::metadata*>(p); }
size_t ht_meta_size(void const* p) { return static_cast<ht::metadata const*>(p)->bytes.size(); }
char const* ht_meta_bytes(void const* p) { return static_cast<ht::metadata const*>(p)->bytes.data(); }
int ht_meta_file_count(void const* p) { return static_cast<ht::metadata const*>(p)->params.ti->num_files(); }
char const* ht_meta_file_path(void const* p, int i) { return static_cast<ht::metadata const*>(p)->paths.at(i).c_str(); }
uint64_t ht_meta_file_size(void const* p, int i) {
    return static_cast<ht::metadata const*>(p)->params.ti->layout().file_size(libtorrent::file_index_t(i));
}
int ht_meta_file_flags(void const* p, int i) {
    return static_cast<ht::metadata const*>(p)->params.ti->layout().pad_file_at(libtorrent::file_index_t(i)) ? 1 : 0;
}
int ht_meta_piece_length(void const* p) { return static_cast<ht::metadata const*>(p)->params.ti->piece_length(); }
char const* ht_meta_magnet(void const* p) { return static_cast<ht::metadata const*>(p)->magnet.c_str(); }
}
