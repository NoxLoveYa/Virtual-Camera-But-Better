//! `IMFAttributes` delegation. Frame Server treats the activator (and the stream) as attribute stores, but
//! `#[implement]` has no built-in aggregation, so every method forwards to an inner store from
//! `MFCreateAttributes`. One macro, used by the activator and the stream.

/// Implement `IMFAttributes_Impl` for `$ty` by forwarding to `self.$field` (an `IMFAttributes`).
macro_rules! delegate_attributes {
    ($ty:ty, $field:ident) => {
        impl ::windows::Win32::Media::MediaFoundation::IMFAttributes_Impl for $ty {
            fn GetItem(
                &self,
                guidkey: *const ::windows_core::GUID,
                pvalue: *mut ::windows::Win32::System::Com::StructuredStorage::PROPVARIANT,
            ) -> ::windows_core::Result<()> {
                $crate::guard::guard("IMFAttributes", || unsafe {
                    self.$field.GetItem(guidkey, Some(pvalue))
                })
            }
            fn GetItemType(
                &self,
                guidkey: *const ::windows_core::GUID,
            ) -> ::windows_core::Result<::windows::Win32::Media::MediaFoundation::MF_ATTRIBUTE_TYPE>
            {
                $crate::guard::guard("IMFAttributes", || unsafe {
                    self.$field.GetItemType(guidkey)
                })
            }
            fn CompareItem(
                &self,
                guidkey: *const ::windows_core::GUID,
                value: *const ::windows::Win32::System::Com::StructuredStorage::PROPVARIANT,
            ) -> ::windows_core::Result<::windows_core::BOOL> {
                $crate::guard::guard("IMFAttributes", || unsafe {
                    self.$field.CompareItem(guidkey, value)
                })
            }
            fn Compare(
                &self,
                ptheirs: ::windows_core::Ref<
                    ::windows::Win32::Media::MediaFoundation::IMFAttributes,
                >,
                matchtype: ::windows::Win32::Media::MediaFoundation::MF_ATTRIBUTES_MATCH_TYPE,
            ) -> ::windows_core::Result<::windows_core::BOOL> {
                $crate::guard::guard("IMFAttributes", || unsafe {
                    self.$field.Compare(ptheirs.as_ref(), matchtype)
                })
            }
            fn GetUINT32(
                &self,
                guidkey: *const ::windows_core::GUID,
            ) -> ::windows_core::Result<u32> {
                $crate::guard::guard("IMFAttributes", || unsafe {
                    self.$field.GetUINT32(guidkey)
                })
            }
            fn GetUINT64(
                &self,
                guidkey: *const ::windows_core::GUID,
            ) -> ::windows_core::Result<u64> {
                $crate::guard::guard("IMFAttributes", || unsafe {
                    self.$field.GetUINT64(guidkey)
                })
            }
            fn GetDouble(
                &self,
                guidkey: *const ::windows_core::GUID,
            ) -> ::windows_core::Result<f64> {
                $crate::guard::guard("IMFAttributes", || unsafe {
                    self.$field.GetDouble(guidkey)
                })
            }
            fn GetGUID(
                &self,
                guidkey: *const ::windows_core::GUID,
            ) -> ::windows_core::Result<::windows_core::GUID> {
                $crate::guard::guard("IMFAttributes", || unsafe { self.$field.GetGUID(guidkey) })
            }
            fn GetStringLength(
                &self,
                guidkey: *const ::windows_core::GUID,
            ) -> ::windows_core::Result<u32> {
                $crate::guard::guard("IMFAttributes", || unsafe {
                    self.$field.GetStringLength(guidkey)
                })
            }
            fn GetString(
                &self,
                guidkey: *const ::windows_core::GUID,
                pwszvalue: ::windows_core::PWSTR,
                cchbufsize: u32,
                pcchlength: *mut u32,
            ) -> ::windows_core::Result<()> {
                // raw vtable call: the caller's pointer and size are passed through untouched
                $crate::guard::guard("IMFAttributes", || unsafe {
                    (::windows_core::Interface::vtable(&self.$field).GetString)(
                        ::windows_core::Interface::as_raw(&self.$field),
                        guidkey,
                        pwszvalue,
                        cchbufsize,
                        pcchlength,
                    )
                    .ok()
                })
            }
            fn GetAllocatedString(
                &self,
                guidkey: *const ::windows_core::GUID,
                ppwszvalue: *mut ::windows_core::PWSTR,
                pcchlength: *mut u32,
            ) -> ::windows_core::Result<()> {
                $crate::guard::guard("IMFAttributes", || unsafe {
                    self.$field
                        .GetAllocatedString(guidkey, ppwszvalue, pcchlength)
                })
            }
            fn GetBlobSize(
                &self,
                guidkey: *const ::windows_core::GUID,
            ) -> ::windows_core::Result<u32> {
                $crate::guard::guard("IMFAttributes", || unsafe {
                    self.$field.GetBlobSize(guidkey)
                })
            }
            fn GetBlob(
                &self,
                guidkey: *const ::windows_core::GUID,
                pbuf: *mut u8,
                cbbufsize: u32,
                pcbblobsize: *mut u32,
            ) -> ::windows_core::Result<()> {
                $crate::guard::guard("IMFAttributes", || unsafe {
                    (::windows_core::Interface::vtable(&self.$field).GetBlob)(
                        ::windows_core::Interface::as_raw(&self.$field),
                        guidkey,
                        pbuf,
                        cbbufsize,
                        pcbblobsize,
                    )
                    .ok()
                })
            }
            fn GetAllocatedBlob(
                &self,
                guidkey: *const ::windows_core::GUID,
                ppbuf: *mut *mut u8,
                pcbsize: *mut u32,
            ) -> ::windows_core::Result<()> {
                $crate::guard::guard("IMFAttributes", || unsafe {
                    self.$field.GetAllocatedBlob(guidkey, ppbuf, pcbsize)
                })
            }
            fn GetUnknown(
                &self,
                guidkey: *const ::windows_core::GUID,
                riid: *const ::windows_core::GUID,
                ppv: *mut *mut ::core::ffi::c_void,
            ) -> ::windows_core::Result<()> {
                $crate::guard::guard("IMFAttributes", || unsafe {
                    (::windows_core::Interface::vtable(&self.$field).GetUnknown)(
                        ::windows_core::Interface::as_raw(&self.$field),
                        guidkey,
                        riid,
                        ppv,
                    )
                    .ok()
                })
            }
            fn SetItem(
                &self,
                guidkey: *const ::windows_core::GUID,
                value: *const ::windows::Win32::System::Com::StructuredStorage::PROPVARIANT,
            ) -> ::windows_core::Result<()> {
                $crate::guard::guard("IMFAttributes", || unsafe {
                    self.$field.SetItem(guidkey, value)
                })
            }
            fn DeleteItem(
                &self,
                guidkey: *const ::windows_core::GUID,
            ) -> ::windows_core::Result<()> {
                $crate::guard::guard("IMFAttributes", || unsafe {
                    self.$field.DeleteItem(guidkey)
                })
            }
            fn DeleteAllItems(&self) -> ::windows_core::Result<()> {
                $crate::guard::guard("IMFAttributes", || unsafe { self.$field.DeleteAllItems() })
            }
            fn SetUINT32(
                &self,
                guidkey: *const ::windows_core::GUID,
                unvalue: u32,
            ) -> ::windows_core::Result<()> {
                $crate::guard::guard("IMFAttributes", || unsafe {
                    self.$field.SetUINT32(guidkey, unvalue)
                })
            }
            fn SetUINT64(
                &self,
                guidkey: *const ::windows_core::GUID,
                unvalue: u64,
            ) -> ::windows_core::Result<()> {
                $crate::guard::guard("IMFAttributes", || unsafe {
                    self.$field.SetUINT64(guidkey, unvalue)
                })
            }
            fn SetDouble(
                &self,
                guidkey: *const ::windows_core::GUID,
                fvalue: f64,
            ) -> ::windows_core::Result<()> {
                $crate::guard::guard("IMFAttributes", || unsafe {
                    self.$field.SetDouble(guidkey, fvalue)
                })
            }
            fn SetGUID(
                &self,
                guidkey: *const ::windows_core::GUID,
                guidvalue: *const ::windows_core::GUID,
            ) -> ::windows_core::Result<()> {
                $crate::guard::guard("IMFAttributes", || unsafe {
                    self.$field.SetGUID(guidkey, guidvalue)
                })
            }
            fn SetString(
                &self,
                guidkey: *const ::windows_core::GUID,
                wszvalue: &::windows_core::PCWSTR,
            ) -> ::windows_core::Result<()> {
                $crate::guard::guard("IMFAttributes", || unsafe {
                    self.$field.SetString(guidkey, *wszvalue)
                })
            }
            fn SetBlob(
                &self,
                guidkey: *const ::windows_core::GUID,
                pbuf: *const u8,
                cbbufsize: u32,
            ) -> ::windows_core::Result<()> {
                $crate::guard::guard("IMFAttributes", || unsafe {
                    (::windows_core::Interface::vtable(&self.$field).SetBlob)(
                        ::windows_core::Interface::as_raw(&self.$field),
                        guidkey,
                        pbuf,
                        cbbufsize,
                    )
                    .ok()
                })
            }
            fn SetUnknown(
                &self,
                guidkey: *const ::windows_core::GUID,
                punknown: ::windows_core::Ref<::windows_core::IUnknown>,
            ) -> ::windows_core::Result<()> {
                $crate::guard::guard("IMFAttributes", || unsafe {
                    self.$field.SetUnknown(guidkey, punknown.as_ref())
                })
            }
            fn LockStore(&self) -> ::windows_core::Result<()> {
                $crate::guard::guard("IMFAttributes", || unsafe { self.$field.LockStore() })
            }
            fn UnlockStore(&self) -> ::windows_core::Result<()> {
                $crate::guard::guard("IMFAttributes", || unsafe { self.$field.UnlockStore() })
            }
            fn GetCount(&self) -> ::windows_core::Result<u32> {
                $crate::guard::guard("IMFAttributes", || unsafe { self.$field.GetCount() })
            }
            fn GetItemByIndex(
                &self,
                unindex: u32,
                pguidkey: *mut ::windows_core::GUID,
                pvalue: *mut ::windows::Win32::System::Com::StructuredStorage::PROPVARIANT,
            ) -> ::windows_core::Result<()> {
                $crate::guard::guard("IMFAttributes", || unsafe {
                    self.$field.GetItemByIndex(unindex, pguidkey, Some(pvalue))
                })
            }
            fn CopyAllItems(
                &self,
                pdest: ::windows_core::Ref<::windows::Win32::Media::MediaFoundation::IMFAttributes>,
            ) -> ::windows_core::Result<()> {
                $crate::guard::guard("IMFAttributes", || unsafe {
                    self.$field.CopyAllItems(pdest.as_ref())
                })
            }
        }
    };
}

pub(crate) use delegate_attributes;
