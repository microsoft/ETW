// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

/*
Internal implementation details for EtwEnumerator.h.
*/

#pragma once

namespace EtwInternal
{
    /*
    Default constructor for Buffer<T, N> where N != 0.
    Calls the Buffer<T, 0> constructor which sets initial capacity to N.
    */
    template<class T, unsigned StaticCapacity>
    Buffer<T, StaticCapacity>::Buffer() noexcept
        : Buffer<T, 0>(StaticCapacity)
    {
        static_assert(StaticCapacity <= Buffer<T, 0>::MaxCapacity, "StaticCapacity is too large.");
        return;
    }

    /*
    Default constructor for Buffer<T, 0>.
    Sets initial capacity to 0.
    */
    template<class T>
    constexpr
    Buffer<T, 0>::Buffer() noexcept
        : m_pData(nullptr)
        , m_size(0)
        , m_capacity(0)
    {
        return;
    }

    /*
    Protected constructor for Buffer<T, 0>, called by Buffer<T, N> constructor.
    Sets initial capacity to N.
    */
    template<class T>
    Buffer<T, 0>::Buffer(
        size_type staticCapacity) noexcept
        : m_pData(reinterpret_cast<T*>(this + 1))
        , m_size(0)
        , m_capacity(staticCapacity)
    {
        return;
    }

    template<class T>
    Buffer<T, 0>::~Buffer() noexcept
    {
        if (m_pData != nullptr &&
            m_pData != reinterpret_cast<T*>(this + 1))
        {
			HeapFree(GetProcessHeap(), 0, m_pData);
        }
    }

    template<class T>
    typename Buffer<T, 0>::size_type
    Buffer<T, 0>::size() const noexcept
    {
        return m_size;
    }

    template<class T>
    typename Buffer<T, 0>::size_type
    Buffer<T, 0>::byte_size() const noexcept
    {
        return m_size * sizeof(T);
    }

    template<class T>
    typename Buffer<T, 0>::size_type
    Buffer<T, 0>::capacity() const noexcept
    {
        return m_capacity;
    }

    template<class T>
    T const*
    Buffer<T, 0>::data() const noexcept
    {
        return m_pData;
    }

    template<class T>
    T*
    Buffer<T, 0>::data() noexcept
    {
        return m_pData;
    }

    template<class T>
    T*
    Buffer<T, 0>::begin() noexcept
    {
        return m_pData;
    }

    template<class T>
    T*
    Buffer<T, 0>::end() noexcept
    {
        return m_pData + m_size;
    }

    template<class T>
    T const&
    Buffer<T, 0>::operator[](
        size_type i) const noexcept
    {
        ASSERT(i < m_size);
        return m_pData[i];
    }

    template<class T>
    T&
    Buffer<T, 0>::operator[](
        size_type i) noexcept
    {
        ASSERT(i < m_size);
        return m_pData[i];
    }

    template<class T>
    void
    Buffer<T, 0>::clear() noexcept
    {
        m_size = 0;
    }

    template<class T>
    void
    Buffer<T, 0>::resize_unchecked(size_type newSize) noexcept
    {
        ASSERT(newSize <= m_capacity);
        m_size = newSize;
    }

    template<class T>
    bool
    Buffer<T, 0>::push_back(
        T const& value) noexcept
    {
        bool ok;

        if (m_size < m_capacity || Grow(m_size + 1, true))
        {
            m_pData[m_size] = value;
            m_size += 1;
            ok = true;
        }
        else
        {
            ok = false;
        }

        return ok;
    }

    template<class T>
    void
    Buffer<T, 0>::pop_back() noexcept
    {
        ASSERT(m_size != 0);
        m_size -= 1;
    }

    template<class T>
    bool
    Buffer<T, 0>::reserve(
        size_type requiredCapacity,
        bool keepExistingData) noexcept
    {
        bool ok =
            requiredCapacity <= m_capacity ||
            Grow(requiredCapacity, keepExistingData);
        return ok;
    }

    template<class T>
    bool
    Buffer<T, 0>::resize(
        size_type newSize,
        bool keepExistingData) noexcept
    {
        bool ok;

        if (newSize <= m_capacity ||
            Grow(newSize, keepExistingData))
        {
            m_size = newSize;
            ok = true;
        }
        else
        {
            ok = false;
        }

        return ok;
    }

    template<class T>
    bool
    Buffer<T, 0>::Grow(
        size_type requiredCapacity,
        bool keepExistingData) noexcept
    {
        __analysis_assume(m_size < requiredCapacity);
        ASSERT(m_capacity < requiredCapacity);
        ASSERT(m_size <= m_capacity);

        bool ok;
        size_type newCapacity;
        T* pNewData;

        newCapacity = m_capacity != 0 ? m_capacity : 8;
        do
        {
            if (newCapacity > MaxCapacity / 2u)
            {
                if (requiredCapacity <= MaxCapacity)
                {
                    newCapacity = MaxCapacity;
                    break;
                }

                ok = false;
                goto Done;
            }

            newCapacity *= 2;
        } while (newCapacity < requiredCapacity);

        pNewData = static_cast<T*>(HeapAlloc(GetProcessHeap(), 0, newCapacity * sizeof(T)));
        if (pNewData == nullptr)
        {
            ok = false;
            goto Done;
        }

        if (keepExistingData && m_size != 0)
        {
            memcpy(pNewData, m_pData, m_size * sizeof(T));
        }

        if (m_pData != nullptr &&
            m_pData != reinterpret_cast<T*>(this + 1))
        {
			HeapFree(GetProcessHeap(), 0, m_pData);
		}

        m_pData = pNewData;
        m_capacity = newCapacity;
        ok = true;

    Done:

        return ok;
    }
}
// namespace EtwInternal
