#pragma once

#include <condition_variable>
#include <memory>
#include <mutex>
#include <utility>
#include "api/frame_transformer_interface.h"
#include "api/scoped_refptr.h"

namespace pulsebeam::webrtc_sys {

// For the pinned SDK delegate callbacks only. Reset unregisters before tearing
// down its target/borrowed queue. Retirement waits for calls already admitted;
// a late completion holding this slot cannot enter a retired callback.
//
// Do not hold a bridge mutex across the SDK call: its queue's PostTask may
// synchronously invoke an injected Rust scheduler's admission callback.
class StockFrameCallbackSlot final {
 public:
  explicit StockFrameCallbackSlot(
      webrtc::scoped_refptr<webrtc::TransformedFrameCallback> callback)
      : callback_(std::move(callback)) {}

  bool OnTransformedFrame(
      std::unique_ptr<webrtc::TransformableFrameInterface> frame) {
    auto callback = Admit();
    if (!callback) return false;
    callback->OnTransformedFrame(std::move(frame));
    // Drop the call's reference before retirement can return. Final delegate
    // release stays on the SDK unregister role, not a completion caller.
    callback = nullptr;
    Complete();
    return true;
  }

  bool StartShortCircuiting() {
    auto callback = Admit();
    if (!callback) return false;
    callback->StartShortCircuiting();
    callback = nullptr;
    Complete();
    return true;
  }

  bool IsLive() {
    std::lock_guard lock(mutex_);
    return !retired_ && callback_ != nullptr;
  }

  void Retire() {
    webrtc::scoped_refptr<webrtc::TransformedFrameCallback> retired;
    {
      std::unique_lock lock(mutex_);
      retired_ = true;
      idle_.wait(lock, [this] { return active_ == 0; });
      retired = std::move(callback_);
    }
    // Never release the final SDK reference under a bridge lock.
  }

 private:
  webrtc::scoped_refptr<webrtc::TransformedFrameCallback> Admit() {
    std::lock_guard lock(mutex_);
    if (retired_ || !callback_) return nullptr;
    ++active_;
    return callback_;
  }

  void Complete() {
    std::lock_guard lock(mutex_);
    --active_;
    if (active_ == 0) idle_.notify_all();
  }

  std::mutex mutex_;
  std::condition_variable idle_;
  webrtc::scoped_refptr<webrtc::TransformedFrameCallback> callback_;
  std::size_t active_ = 0;
  bool retired_ = false;
};
}  // namespace pulsebeam::webrtc_sys
