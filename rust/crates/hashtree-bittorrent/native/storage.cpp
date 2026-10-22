#include "internal.hpp"
#include <libtorrent/disk_buffer_holder.hpp>
#include <libtorrent/hasher.hpp>
#include <libtorrent/peer_request.hpp>
#include <boost/asio/post.hpp>
#include <boost/asio/thread_pool.hpp>
#include <boost/asio/executor_work_guard.hpp>
#include <atomic>
#include <cstdlib>
#include <cstring>

namespace ht {
namespace {
lt::storage_error io_error() {
    return lt::storage_error(lt::error_code(boost::system::errc::io_error, lt::generic_category()),
        lt::operation_t::file_read);
}
struct storage {
    lt::file_storage files;
    std::shared_ptr<source> src;
    std::shared_ptr<cache> data;

    std::vector<char> read(lt::peer_request const& r) {
        int const size = files.piece_size(r.piece);
        if (r.start < 0 || r.length < 0 || r.length > max_piece
            || r.start > size || r.length > size - r.start)
            throw failure("invalid storage range");
        std::vector<char> result(std::size_t(r.length), 0);
        if (src) {
            if (src->cancelled && src->cancelled(src->cancel_context)) throw failure("cancelled", 2);
            std::size_t written = 0;
            for (auto const& slice : files.map_block(r.piece, r.start, r.length)) {
                if (!files.pad_file_at(slice.file_index)) {
                    auto const file = src->files.find(files.file_path(slice.file_index));
                    if (file == src->files.end() || src->read(src->context, file->second,
                        std::uint64_t(slice.offset), std::uint32_t(slice.size), result.data() + written) != 0)
                        throw failure("snapshot read failed");
                }
                written += std::size_t(slice.size);
            }
            if (written != result.size()) throw failure("short snapshot read");
        } else {
            std::lock_guard<std::mutex> lock(data->mutex);
            auto p = data->pieces.find(int(r.piece));
            if (p == data->pieces.end() || p->second.data.size() < std::size_t(r.start + r.length))
                throw failure("piece is not cached");
            std::memcpy(result.data(), p->second.data.data() + r.start, result.size());
            p->second.age = ++data->clock;
        }
        return result;
    }

    void write(lt::peer_request const& r, std::vector<char> const& bytes) {
        if (src || !data) throw failure("snapshot storage is read-only");
        int const size = files.piece_size(r.piece);
        if (r.start < 0 || r.length <= 0 || r.start > size || r.length > size - r.start)
            throw failure("invalid piece write");
        std::lock_guard<std::mutex> lock(data->mutex);
        if (!data->wanted.count(int(r.piece))) throw failure("unsolicited piece write");
        auto p = data->pieces.find(int(r.piece));
        if (p == data->pieces.end()) {
            if (std::size_t(size) > data->limit - data->bytes) throw failure("piece cache is full");
            piece entry;
            entry.data.resize(std::size_t(size), 0);
            p = data->pieces.emplace(int(r.piece), std::move(entry)).first;
            data->bytes += std::size_t(size);
        }
        std::memcpy(p->second.data.data() + r.start, bytes.data(), bytes.size());
        p->second.age = ++data->clock;
    }
};

class disk final : public lt::disk_interface, public lt::buffer_allocator_interface {
    lt::io_context& io;
    boost::asio::executor_work_guard<lt::io_context::executor_type> work;
    boost::asio::thread_pool worker{1};
    std::atomic<bool> joined{false};
    std::map<std::uint32_t, std::shared_ptr<storage>> stores;
    std::uint32_t next = 0;
    std::shared_ptr<source> src;
    std::shared_ptr<cache> data;
public:
    disk(lt::io_context& i, std::shared_ptr<source> s, std::shared_ptr<cache> c)
        : io(i), work(boost::asio::make_work_guard(i)), src(std::move(s)), data(std::move(c)) {}
    ~disk() override { abort(true); }
    lt::storage_holder new_torrent(lt::storage_params const& p, std::shared_ptr<void> const&) override {
        auto id = next++;
        stores.emplace(id, std::make_shared<storage>(storage{p.files, src, data}));
        return lt::storage_holder(lt::storage_index_t(id), *this);
    }
    void remove_torrent(lt::storage_index_t s) override { stores.erase(std::uint32_t(s)); }
    void settings_updated() override {}
    void submit_jobs() override {}
    void update_stats_counters(lt::counters&) const override {}
    std::vector<lt::open_file_state> get_status(lt::storage_index_t) const override { return {}; }
    void abort(bool wait) override {
        if (wait && !joined.exchange(true)) worker.join();
        work.reset();
    }

    void async_read(lt::storage_index_t s, lt::peer_request const& r,
        std::function<void(lt::disk_buffer_holder, lt::storage_error const&)> done, lt::disk_job_flags_t) override {
        auto st = stores.at(std::uint32_t(s));
        boost::asio::post(worker, [this, st, r, done=std::move(done)] {
            char* buffer = nullptr;
            lt::storage_error error;
            try {
                auto bytes = st->read(r);
                buffer = static_cast<char*>(std::malloc(bytes.size()));
                if (!buffer) throw std::bad_alloc();
                std::memcpy(buffer, bytes.data(), bytes.size());
            } catch (...) { error = io_error(); }
            boost::asio::post(io, [this, buffer, error, done] {
                done(lt::disk_buffer_holder(*this, buffer), error);
            });
        });
    }
    bool async_write(lt::storage_index_t s, lt::peer_request const& r, char const* bytes,
        std::shared_ptr<lt::disk_observer>, std::function<void(lt::storage_error const&)> done,
        lt::disk_job_flags_t) override {
        auto st = stores.at(std::uint32_t(s));
        std::vector<char> copy(bytes, bytes + r.length);
        boost::asio::post(worker, [this, st, r, copy=std::move(copy), done=std::move(done)] {
            lt::storage_error error;
            try { st->write(r, copy); } catch (...) { error = io_error(); }
            boost::asio::post(io, [done, error] { done(error); });
        });
        return false;
    }
    void async_hash(lt::storage_index_t s, lt::piece_index_t p, lt::span<lt::sha256_hash> v2,
        lt::disk_job_flags_t flags,
        std::function<void(lt::piece_index_t, lt::sha1_hash const&, lt::storage_error const&)> done) override {
        auto st = stores.at(std::uint32_t(s));
        boost::asio::post(worker, [this, st, p, v2, flags, done=std::move(done)] {
            lt::sha1_hash hash;
            lt::storage_error error;
            try {
                auto bytes = st->read({p, 0, st->files.piece_size(p)});
                if (flags & lt::disk_interface::v1_hash) hash = lt::hasher(bytes).final();
                int const size = st->files.piece_size2(p);
                for (int i = 0; i < st->files.blocks_in_piece2(p) && i < v2.size(); ++i) {
                    int const begin = i * lt::default_block_size;
                    v2[i] = lt::hasher256(lt::span<char const>{bytes.data() + begin,
                        std::min(lt::default_block_size, size - begin)}).final();
                }
            } catch (...) { error = io_error(); }
            boost::asio::post(io, [done, p, hash, error] { done(p, hash, error); });
        });
    }
    void async_hash2(lt::storage_index_t s, lt::piece_index_t p, int offset, lt::disk_job_flags_t,
        std::function<void(lt::piece_index_t, lt::sha256_hash const&, lt::storage_error const&)> done) override {
        auto st = stores.at(std::uint32_t(s));
        boost::asio::post(worker, [this, st, p, offset, done=std::move(done)] {
            lt::sha256_hash hash;
            lt::storage_error error;
            try {
                int const size = st->files.piece_size2(p);
                if (offset < 0 || offset >= size) throw failure("invalid block hash range");
                auto bytes = st->read({p, offset, std::min(lt::default_block_size, size - offset)});
                hash = lt::hasher256(bytes).final();
            } catch (...) { error = io_error(); }
            boost::asio::post(io, [done, p, hash, error] { done(p, hash, error); });
        });
    }
    // These operations never touch the filesystem. Completion is posted on the
    // network executor, as required by libtorrent's disk interface.
    void async_check_files(lt::storage_index_t, lt::add_torrent_params const*,
        lt::aux::vector<std::string, lt::file_index_t>,
        std::function<void(lt::status_t, lt::storage_error const&)> done) override {
        boost::asio::post(io, [done] { done(lt::status_t{}, {}); });
    }
    void async_release_files(lt::storage_index_t, std::function<void()> done) override {
        boost::asio::post(worker, [this, done] { if (done) boost::asio::post(io, done); });
    }
    void async_stop_torrent(lt::storage_index_t s, std::function<void()> done) override {
        async_release_files(s, std::move(done));
    }
    void async_move_storage(lt::storage_index_t, std::string path, lt::move_flags_t,
        std::function<void(lt::status_t, std::string const&, lt::storage_error const&)> done) override {
        boost::asio::post(io, [done, path] { done(lt::disk_status::fatal_disk_error, path, io_error()); });
    }
    void async_rename_file(lt::storage_index_t, lt::file_index_t file, std::string name,
        std::function<void(std::string const&, lt::file_index_t, lt::storage_error const&)> done) override {
        boost::asio::post(io, [done, name, file] { done(name, file, io_error()); });
    }
    void async_delete_files(lt::storage_index_t, lt::remove_flags_t,
        std::function<void(lt::storage_error const&)> done) override {
        boost::asio::post(io, [done] { done(io_error()); });
    }
    void async_set_file_priority(lt::storage_index_t,
        lt::aux::vector<lt::download_priority_t, lt::file_index_t> priorities,
        std::function<void(lt::storage_error const&, lt::aux::vector<lt::download_priority_t, lt::file_index_t>)> done) override {
        boost::asio::post(io, [done, priorities] { done({}, priorities); });
    }
    void async_clear_piece(lt::storage_index_t, lt::piece_index_t p,
        std::function<void(lt::piece_index_t)> done) override {
        auto c = data;
        boost::asio::post(worker, [this, c, p, done] {
            if (c) {
                std::lock_guard<std::mutex> lock(c->mutex);
                auto entry = c->pieces.find(int(p));
                if (entry != c->pieces.end()) {
                    c->bytes -= entry->second.data.size();
                    c->pieces.erase(entry);
                }
            }
            boost::asio::post(io, [done, p] { done(p); });
        });
    }
    void free_disk_buffer(char* p) override { std::free(p); }
    void free_multiple_buffers(lt::span<char*> buffers) override {
        for (auto p : buffers) std::free(p);
    }
#if TORRENT_DEBUG_BUFFER_POOL
    void rename_buffer(char*, char const*) override {}
#endif
};
}
lt::disk_io_constructor_type storage_factory(std::shared_ptr<source> src, std::shared_ptr<cache> data) {
    return [src=std::move(src), data=std::move(data)](lt::io_context& io,
        lt::settings_interface const&, lt::counters&) {
        return std::make_unique<disk>(io, src, data);
    };
}
}
