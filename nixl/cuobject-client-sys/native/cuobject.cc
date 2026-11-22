#include "rustfs-cuobject-client-sys/src/sdk.rs.h"
#include <cuobjclient.h>

#include <cerrno>
#include <cstring>
#include <mutex>
#include <stdexcept>
#include <system_error>
#include <sys/mman.h>
#include <utility>

namespace rc::cuobject {

class Client final {
public:
    // Only the descriptor API is used. The signed S3 control exchange runs in Rust.
    CUObjOps_t ops{};
    cuObjClient sdk{ops, CUOBJ_PROTO_RDMA_DC_V1};
    std::mutex mutex;
};

std::shared_ptr<Client> make_client() { return std::make_shared<Client>(); }

static void check(cuObjErr_t status, const char *operation) {
    if (status != CU_OBJ_SUCCESS) {
        throw std::runtime_error(std::string(operation) + " failed with cuObject status " +
                                 std::to_string(static_cast<int>(status)));
    }
}

struct NativeWindow::Registration {
    rust::Box<RegistrationOwner> owner;
    std::shared_ptr<Client> client;
    std::size_t size;
    std::uint8_t *data;
    char *token = nullptr;
    bool release_failed = false;

    Registration(std::shared_ptr<Client> sdk_client, std::size_t length, rust::Box<RegistrationOwner> guard)
        : owner(std::move(guard)), client(std::move(sdk_client)), size(length) {
        if (!client) {
            throw std::invalid_argument("missing cuObject client");
        }
        if (length == 0) {
            throw std::invalid_argument("invalid cuObject registration length");
        }
        void *mapping = mmap(nullptr, length, PROT_READ | PROT_WRITE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (mapping == MAP_FAILED) {
            throw std::system_error(errno, std::generic_category(), "mmap");
        }
        data = static_cast<std::uint8_t *>(mapping);
        try {
            std::lock_guard lock(client->mutex);
            check(client->sdk.cuMemObjGetDescriptor(data, size), "register memory");
        } catch (...) {
            munmap(data, size);
            throw;
        }
    }

    ~Registration() { munmap(data, size); }
};

NativeWindow::NativeWindow(std::shared_ptr<Client> client, std::size_t size, rust::Box<RegistrationOwner> owner)
    : registration_(std::make_unique<Registration>(std::move(client), size, std::move(owner))) {}

NativeWindow::~NativeWindow() {
    auto &r = *registration_;
    // An active token or a failed deregistration must keep the client and mapping alive.
    if (!r.token && !r.release_failed) {
        try {
            std::lock_guard lock(r.client->mutex);
            if (r.client->sdk.cuMemObjPutDescriptor(r.data) == CU_OBJ_SUCCESS) {
                return;
            }
        } catch (...) {
            // The SDK did not confirm deregistration.
        }
    }
    registration_.release();
}

std::unique_ptr<NativeWindow> register_window(std::shared_ptr<Client> client, std::size_t size, rust::Box<RegistrationOwner> owner) {
    return std::make_unique<NativeWindow>(std::move(client), size, std::move(owner));
}

rust::Slice<std::uint8_t> NativeWindow::writable() {
    auto &r = *registration_;
    if (r.token || r.release_failed) {
        throw std::logic_error("cuObject window still exposed");
    }
    return {r.data, r.size};
}

void NativeWindow::acquire(std::size_t length, bool for_get) {
    auto &r = *registration_;
    if (r.token || r.release_failed || length == 0 || length > r.size) {
        throw std::invalid_argument("invalid cuObject token request");
    }
    std::lock_guard lock(r.client->mutex);
    check(r.client->sdk.cuMemObjGetRDMAToken(r.data, length, 0, for_get ? CUOBJ_GET : CUOBJ_PUT,
                                            &r.token), "acquire token");
    if (!r.token || !*r.token) {
        throw std::runtime_error("cuObject returned an empty token");
    }
}

rust::Slice<const std::uint8_t> NativeWindow::active_token() const noexcept {
    const char *token = registration_->token;
    if (!token) {
        return {};
    }
    return {reinterpret_cast<const std::uint8_t *>(token), std::strlen(token)};
}

void NativeWindow::release_token() {
    auto &r = *registration_;
    if (r.release_failed) {
        throw std::runtime_error("previous cuObject token release failed");
    }
    if (r.token) {
        std::lock_guard lock(r.client->mutex);
        // The SDK may consume the string even when it reports a failure.
        r.release_failed = true;
        check(r.client->sdk.cuMemObjPutRDMAToken(std::exchange(r.token, nullptr)), "release token");
        r.release_failed = false;
    }
}

} // namespace rc::cuobject
