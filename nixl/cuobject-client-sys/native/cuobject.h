#pragma once

#include "rust/cxx.h"
#include <cstddef>
#include <cstdint>
#include <memory>

namespace rc::cuobject {

struct RegistrationOwner;
class Client;
std::shared_ptr<Client> make_client();

class NativeWindow final {
public:
    NativeWindow(std::shared_ptr<Client> client, std::size_t size, rust::Box<RegistrationOwner> owner);
    ~NativeWindow();
    rust::Slice<std::uint8_t> writable();
    void acquire(std::size_t length, bool for_get);
    rust::Slice<const std::uint8_t> active_token() const noexcept;
    void release_token();

private:
    struct Registration;
    std::unique_ptr<Registration> registration_;
};

std::unique_ptr<NativeWindow> register_window(std::shared_ptr<Client> client, std::size_t size, rust::Box<RegistrationOwner> owner);

} // namespace rc::cuobject
