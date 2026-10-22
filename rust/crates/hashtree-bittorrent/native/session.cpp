#include "internal.hpp"
#include <libtorrent/alert_types.hpp>
#include <libtorrent/magnet_uri.hpp>
#include <libtorrent/session.hpp>
#include <libtorrent/settings_pack.hpp>
#include <libtorrent/torrent_status.hpp>
#include <libtorrent/write_resume_data.hpp>
#include <algorithm>
#include <cstring>

namespace ht {
lt::settings_pack settings(ht_options const& o, bool seed) {
    lt::settings_pack s;
    s.set_str(lt::settings_pack::listen_interfaces, o.listen);
    s.set_str(lt::settings_pack::dht_bootstrap_nodes, o.bootstrap);
    s.set_bool(lt::settings_pack::enable_dht, o.dht != 0);
    s.set_bool(lt::settings_pack::enable_lsd, false);
    s.set_bool(lt::settings_pack::enable_upnp, false);
    s.set_bool(lt::settings_pack::enable_natpmp, false);
    // Completing one selected range is not the end of a streaming session.
    // Suppress upload-only signalling and retain peers for subsequent reads.
    s.set_bool(lt::settings_pack::close_redundant_connections, false);
    s.set_bool(lt::settings_pack::announce_to_all_trackers, false);
    s.set_bool(lt::settings_pack::announce_to_all_tiers, false);
    s.set_int(lt::settings_pack::connections_limit, int(o.connections));
    s.set_int(lt::settings_pack::max_peerlist_size, 4000);
    s.set_int(lt::settings_pack::max_paused_peerlist_size, 1000);
    s.set_int(lt::settings_pack::unchoke_slots_limit, seed ? 8 : 0);
    s.set_int(lt::settings_pack::num_optimistic_unchoke_slots, 0);
    s.set_int(lt::settings_pack::upload_rate_limit, int(o.upload_limit));
    s.set_int(lt::settings_pack::download_rate_limit, int(o.download_limit));
    s.set_int(lt::settings_pack::max_out_request_queue, 32);
    s.set_int(lt::settings_pack::max_allowed_in_request_queue, 64);
    s.set_int(lt::settings_pack::send_buffer_watermark, 256 * 1024);
    s.set_int(lt::settings_pack::max_metadata_size, int(max_metadata));
    s.set_int(lt::settings_pack::max_web_seed_connections, 0);
    s.set_int(lt::settings_pack::alert_queue_size, 1024);
    s.set_int(lt::settings_pack::alert_mask, int(std::uint32_t(lt::alert_category::error | lt::alert_category::status
        | lt::alert_category::storage)));
    return s;
}
namespace {
using clock = std::chrono::steady_clock;
struct operation {
    clock::time_point deadline;
    ht_cancel_fn cancelled;
    void* context;
    void check() const {
        if (cancelled && cancelled(context)) throw failure("operation cancelled", 2);
        if (clock::now() >= deadline) throw failure("torrent read timed out", 1);
    }
};
struct engine {
    std::shared_ptr<source> src;
    std::shared_ptr<cache> data;
    std::unique_ptr<lt::session> session;
    lt::torrent_handle handle;
    lt::add_torrent_params base;
    std::vector<lt::tcp::endpoint> peers;
    bool trackers;
    std::uint64_t previous_down = 0, previous_up = 0;
    bool reset_needed = false;
    enum class phase { active, pausing, removing } state = phase::active;
    bool recycling() const { return state != phase::active; }

    engine(lt::add_torrent_params p, ht_options const& o, std::shared_ptr<source> source)
        : src(std::move(source)), data(std::make_shared<cache>()), base(std::move(p)), trackers(o.trackers != 0) {
        data->limit = std::size_t(o.cache_bytes);
        if (base.ti) {
            validate(*base.ti);
            if (!src && std::size_t(base.ti->piece_length()) > data->limit)
                throw failure("piece cache must hold at least one torrent piece");
        }
        lt::session_params parameters(settings(o, bool(src)));
        parameters.disk_io_constructor = storage_factory(src, data);
        session = std::make_unique<lt::session>(std::move(parameters));
        add();
    }
    void add() {
        auto p = base;
        // No web seeds or filesystem storage are used by this adapter.
        p.url_seeds.clear();
        p.dht_nodes.clear();
        if (!trackers) { p.trackers.clear(); p.tracker_tiers.clear(); }
        p.save_path = ".";
        p.flags &= ~(lt::torrent_flags::auto_managed | lt::torrent_flags::paused);
        if (src) {
            p.flags |= lt::torrent_flags::seed_mode | lt::torrent_flags::upload_mode;
            p.have_pieces.resize(p.ti->num_pieces(), true);
            p.verified_pieces.resize(p.ti->num_pieces(), true);
        } else {
            p.flags &= ~(lt::torrent_flags::seed_mode | lt::torrent_flags::upload_mode);
            p.flags |= lt::torrent_flags::default_dont_download;
            // A recycled session discards partial pieces; resume data must not
            // claim their blocks still exist in our memory-only storage.
            p.unfinished_pieces.clear();
            if (p.ti) {
                p.file_priorities.assign(p.ti->num_files(), lt::dont_download);
                p.piece_priorities.assign(p.ti->num_pieces(), lt::dont_download);
                p.have_pieces.resize(p.ti->num_pieces(), false);
                p.have_pieces.clear_all();
                std::lock_guard<std::mutex> lock(data->mutex);
                for (auto const& item : data->pieces)
                    if (item.second.verified) p.have_pieces.set_bit(lt::piece_index_t(item.first));
            }
        }
        lt::error_code ec;
        handle = session->add_torrent(std::move(p), ec);
        if (ec) throw failure(ec.message());
        for (auto const& peer : peers) handle.connect_peer(peer);
    }
    template<class Predicate>
    void wait(operation const& op, Predicate predicate) {
        for (;;) {
            op.check();
            std::vector<lt::alert*> alerts;
            session->pop_alerts(&alerts);
            bool complete = false;
            for (auto alert : alerts) {
                if (auto e = lt::alert_cast<lt::torrent_error_alert>(alert)) throw failure(e->error.message());
                if (auto e = lt::alert_cast<lt::file_error_alert>(alert)) throw failure(e->error.message());
                if (auto e = lt::alert_cast<lt::listen_failed_alert>(alert)) throw failure(e->error.message());
                if (predicate(alert)) complete = true;
            }
            if (complete) return;
            session->wait_for_alert(std::chrono::milliseconds(50));
        }
    }
    lt::add_torrent_params resume(operation const& op) {
        lt::add_torrent_params result;
        handle.save_resume_data(lt::torrent_handle::save_info_dict);
        wait(op, [&](lt::alert* a) {
            if (auto saved = lt::alert_cast<lt::save_resume_data_alert>(a)) {
                result = saved->params;
                return true;
            }
            if (auto failed = lt::alert_cast<lt::save_resume_data_failed_alert>(a))
                throw failure(failed->error.message());
            return false;
        });
        return result;
    }
    void recycle(operation const& op, std::size_t required) {
        // Eviction must also retract the old bitfield. Pause/drain and replace
        // the torrent handle before dropping bytes; retain verified cached pieces.
        // Persist each transition before awaiting its alert. Cancellation may
        // arrive after removal was requested; the next read must finish that
        // transition, not try to use or remove the old handle a second time.
        if (state == phase::active) {
            std::vector<lt::peer_info> connected;
            handle.get_peer_info(connected);
            for (auto const& peer : connected)
                if (peers.size() < 1000 && std::find(peers.begin(), peers.end(), peer.remote_endpoint()) == peers.end())
                    peers.push_back(peer.remote_endpoint());
            base = resume(op);
            auto status = handle.status();
            previous_down += std::uint64_t(status.total_payload_download);
            previous_up += std::uint64_t(status.total_payload_upload);
            if (!(handle.flags() & lt::torrent_flags::paused)) {
                state = phase::pausing;
                handle.pause();
            } else {
                state = phase::removing;
                session->remove_torrent(handle);
            }
        }
        if (state == phase::pausing) {
            wait(op, [](lt::alert* a) { return lt::alert_cast<lt::torrent_paused_alert>(a) != nullptr; });
            state = phase::removing;
            session->remove_torrent(handle);
        }
        wait(op, [](lt::alert* a) { return lt::alert_cast<lt::torrent_removed_alert>(a) != nullptr; });
        {
            std::lock_guard<std::mutex> lock(data->mutex);
            data->wanted.clear();
            for (auto it = data->pieces.begin(); it != data->pieces.end();) {
                if (!it->second.verified) {
                    data->bytes -= it->second.data.size();
                    it = data->pieces.erase(it);
                } else ++it;
            }
            while (required > data->limit - data->bytes) {
                auto oldest = std::min_element(data->pieces.begin(), data->pieces.end(),
                    [](auto const& a, auto const& b) { return a.second.age < b.second.age; });
                if (oldest == data->pieces.end()) throw failure("piece exceeds cache limit");
                data->bytes -= oldest->second.data.size();
                data->pieces.erase(oldest);
            }
        }
        add();
        state = phase::active;
        reset_needed = false;
    }
    void fetch(int index, operation const& op) {
        if (reset_needed || recycling()) recycle(op, 0);
        auto ti = handle.torrent_file();
        auto p = lt::piece_index_t(index);
        bool need_space;
        {
            std::lock_guard<std::mutex> lock(data->mutex);
            auto found = data->pieces.find(index);
            if (found != data->pieces.end() && found->second.verified) {
                found->second.age = ++data->clock;
                return;
            }
            need_space = found == data->pieces.end()
                && std::size_t(ti->layout().piece_size(p)) > data->limit - data->bytes;
        }
        if (need_space) recycle(op, std::size_t(ti->layout().piece_size(p)));
        {
            std::lock_guard<std::mutex> lock(data->mutex);
            data->wanted.insert(index);
        }
        handle.piece_priority(p, lt::top_priority);
        handle.set_piece_deadline(p, 0, lt::torrent_handle::alert_when_available);
        try {
            wait(op, [&](lt::alert* a) {
                auto piece_read = lt::alert_cast<lt::read_piece_alert>(a);
                if (!piece_read || piece_read->piece != p) return false;
                if (piece_read->error) throw failure(piece_read->error.message());
                std::lock_guard<std::mutex> lock(data->mutex);
                auto entry = data->pieces.find(index);
                if (entry == data->pieces.end()) throw failure("verified piece is missing");
                entry->second.verified = true;
                return true;
            });
            handle.reset_piece_deadline(p);
            handle.piece_priority(p, lt::dont_download);
        } catch (...) {
            handle.reset_piece_deadline(p);
            handle.piece_priority(p, lt::dont_download);
            reset_needed = true;
            throw;
        }
    }
};
operation make_operation(std::uint64_t timeout, ht_cancel_fn fn, void* ctx) {
    return {clock::now() + std::chrono::milliseconds(timeout), fn, ctx};
}
}
}
extern "C" {
void* ht_session_open(void const* metadata, ht_options const* opts, ht_file const* files,
    size_t count, ht_read_fn fn, void* ctx) {
    ht::clear_error();
    try {
        auto src = fn ? ht::make_source(files, count, fn, ctx) : nullptr;
        return new ht::engine(static_cast<ht::metadata const*>(metadata)->params, *opts, std::move(src));
    } catch (...) { ht::save_error(); return nullptr; }
}
void* ht_session_magnet(char const* magnet, ht_options const* opts) {
    ht::clear_error();
    try {
        auto p = libtorrent::parse_magnet_uri(magnet);
        return new ht::engine(std::move(p), *opts, nullptr);
    } catch (...) { ht::save_error(); return nullptr; }
}
void* ht_session_metadata(void* ptr, uint64_t timeout, ht_cancel_fn fn, void* ctx) {
    ht::clear_error();
    try {
        auto& e = *static_cast<ht::engine*>(ptr);
        auto op = ht::make_operation(timeout, fn, ctx);
        if (!e.handle.torrent_file()) e.wait(op, [](libtorrent::alert* a) {
            return libtorrent::alert_cast<libtorrent::metadata_received_alert>(a) != nullptr;
        });
        auto params = e.resume(op);
        ht::validate(*params.ti);
        if (std::size_t(params.ti->piece_length()) > e.data->limit) throw ht::failure("piece exceeds cache limit");
        e.base = params;
        return new ht::metadata(std::move(params));
    } catch (...) { ht::save_error(); return nullptr; }
}
void ht_session_free(void* ptr) { delete static_cast<ht::engine*>(ptr); }
int ht_session_connect(void* ptr, char const* ip, uint16_t port) {
    ht::clear_error();
    try {
        auto& e = *static_cast<ht::engine*>(ptr);
        auto address = libtorrent::make_address(ip);
        libtorrent::tcp::endpoint peer(address, port);
        if (e.peers.size() >= 1000) throw ht::failure("peer limit exceeded");
        e.peers.push_back(peer);
        if (!e.recycling()) e.handle.connect_peer(peer);
        return 0;
    } catch (...) { ht::save_error(); return -1; }
}
int ht_session_read(void* ptr, int file, uint64_t offset, uint32_t length, char* out,
    uint64_t timeout, ht_cancel_fn fn, void* ctx) {
    ht::clear_error();
    try {
        auto& e = *static_cast<ht::engine*>(ptr);
        auto ti = e.base.ti;
        if (!ti || e.src || file < 0 || file >= ti->num_files() || length > ht::max_piece)
            throw ht::failure("invalid torrent read");
        auto const& fs = ti->layout();
        auto f = libtorrent::file_index_t(file);
        if (fs.pad_file_at(f) || offset > std::uint64_t(fs.file_size(f))
            || length > std::uint64_t(fs.file_size(f)) - offset)
            throw ht::failure("range exceeds file");
        auto op = ht::make_operation(timeout, fn, ctx);
        std::uint64_t position = std::uint64_t(fs.file_offset(f)) + offset;
        std::uint32_t copied = 0;
        while (copied < length) {
            op.check();
            int const index = int(position / std::uint64_t(ti->piece_length()));
            int const begin = int(position % std::uint64_t(ti->piece_length()));
            int const size = std::min(int(length - copied), fs.piece_size(libtorrent::piece_index_t(index)) - begin);
            e.fetch(index, op);
            {
                std::lock_guard<std::mutex> lock(e.data->mutex);
                auto const& piece = e.data->pieces.at(index);
                if (!piece.verified) throw ht::failure("unverified piece");
                std::memcpy(out + copied, piece.data.data() + begin, std::size_t(size));
            }
            position += std::uint64_t(size);
            copied += std::uint32_t(size);
        }
        return 0;
    } catch (...) { ht::save_error(); return -1; }
}
int ht_session_stats(void* ptr, ht_stats* out) {
    ht::clear_error();
    try {
        auto& e = *static_cast<ht::engine*>(ptr);
        auto s = e.recycling() ? libtorrent::torrent_status{} : e.handle.status();
        if (s.errc) throw ht::failure(s.errc.message());
        out->downloaded = e.previous_down + std::uint64_t(s.total_payload_download);
        out->uploaded = e.previous_up + std::uint64_t(s.total_payload_upload);
        out->peers = std::uint32_t(s.num_peers);
        out->port = e.session->listen_port();
        std::lock_guard<std::mutex> lock(e.data->mutex);
        out->cache_bytes = e.data->bytes;
        out->pieces = std::uint32_t(std::count_if(e.data->pieces.begin(), e.data->pieces.end(),
            [](auto const& p) { return p.second.verified; }));
        return 0;
    } catch (...) { ht::save_error(); return -1; }
}
}
