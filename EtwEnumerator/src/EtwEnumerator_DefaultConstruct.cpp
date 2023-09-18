// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

#include "stdafx.h"
#include <EtwEnumerator.h>

/*
This is in a separate file so that if the user provides their own
implementation of EtwEnumeratorCallbacks they can avoid a dependency on
tdh.lib or DefaultCallbacksInstance.
*/

namespace EtwInternal
{
    // EtwEnumeratorCallbacks used for default-constructed EtwEnumerator:
    struct DefaultCallbacks final : EtwEnumeratorCallbacks {};
}
// namespace EtwInternal

using EtwInternal::DefaultCallbacks;

// constexpr to avoid a dynamic initializer:
static constexpr DefaultCallbacks DefaultCallbacksInstance;

EtwEnumerator::EtwEnumerator() noexcept
    : EtwEnumerator(const_cast<DefaultCallbacks&>(DefaultCallbacksInstance))
{
    return;
}
