// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Registry for codecs to support lookup and instantiation of decoders dynamically at runtime.

use std::collections::HashMap;
use std::default::Default;
use std::hash::Hash;

use crate::codecs::CodecInfo;
use crate::codecs::audio::{AudioCodecId, AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
#[cfg(feature = "exp-subtitle-codecs")]
use crate::codecs::subtitle::{
    SubtitleCodecId, SubtitleCodecParameters, SubtitleDecoder, SubtitleDecoderOptions,
};
#[cfg(feature = "exp-video-codecs")]
use crate::codecs::video::{VideoCodecId, VideoCodecParameters, VideoDecoder, VideoDecoderOptions};
use crate::common::Tier;
use crate::errors::{Result, unsupported_error};

/// Description of a supported audio codec.
#[derive(Copy, Clone)]
pub struct SupportedAudioCodec {
    pub id: AudioCodecId,
    pub info: CodecInfo,
}

/// To support registration in a codec registry, an `AudioDecoder` must implement the
/// `RegisterableAudioDecoder` trait.
pub trait RegisterableAudioDecoder: AudioDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized;

    /// Get a list of audio codecs supported by this decoder.
    fn supported_codecs() -> &'static [SupportedAudioCodec];
}

/// Description of a supported video codec.
#[cfg(feature = "exp-video-codecs")]
#[derive(Copy, Clone)]
pub struct SupportedVideoCodec {
    pub codec: VideoCodecId,
    pub info: CodecInfo,
}

/// To support registration in a codec registry, a `VideoDecoder` must implement the
/// `RegisterableVideoDecoder` trait.
#[cfg(feature = "exp-video-codecs")]
pub trait RegisterableVideoDecoder: VideoDecoder {
    fn try_registry_new(
        params: &VideoCodecParameters,
        opts: &VideoDecoderOptions,
    ) -> Result<Box<dyn VideoDecoder>>
    where
        Self: Sized;

    /// Get a list of video codecs supported by this decoder.
    fn supported_codecs() -> &'static [SupportedVideoCodec];
}

/// Description of a supported subtitle codec.
#[cfg(feature = "exp-subtitle-codecs")]
#[derive(Copy, Clone)]
pub struct SupportedSubtitleCodec {
    pub codec: SubtitleCodecId,
    pub info: CodecInfo,
}

/// To support registration in a codec registry, a `SubtitleDecoder` must implement the
/// `RegisterableSubtitleDecoder` trait.
#[cfg(feature = "exp-subtitle-codecs")]
pub trait RegisterableSubtitleDecoded: SubtitleDecoder {
    fn try_registry_new(
        params: &SubtitleCodecParameters,
        opts: &SubtitleDecoderOptions,
    ) -> Result<Box<dyn SubtitleDecoder>>
    where
        Self: Sized;

    /// Get a list of subtitle codecs supported by this decoder.
    fn supported_codecs() -> &'static [SupportedSubtitleCodec];
}

/// `AudioDecoder` factory function. Creates a boxed `AudioDecoder`.
pub type AudioDecoderFactoryFn =
    fn(&AudioCodecParameters, &AudioDecoderOptions) -> Result<Box<dyn AudioDecoder>>;

/// `VideoDecoder` factory function. Creates a boxed `VideoDecoder`.
#[cfg(feature = "exp-video-codecs")]
pub type VideoDecoderFactoryFn =
    fn(&VideoCodecParameters, &VideoDecoderOptions) -> Result<Box<dyn VideoDecoder>>;

/// `SubtitleDecoder` factory function. Creates a boxed `SubtitleDecoder`.
#[cfg(feature = "exp-subtitle-codecs")]
pub type SubtitleDecoderFactoryFn =
    fn(&SubtitleCodecParameters, &SubtitleDecoderOptions) -> Result<Box<dyn SubtitleDecoder>>;

/// Registration details of an audio decoder for a particular audio codec.
pub struct RegisteredAudioDecoder {
    /// Audio codec details.
    pub codec: SupportedAudioCodec,
    /// Factory function to instantiate the audio decoder.
    pub factory: AudioDecoderFactoryFn,
}

/// Registration details of a video decoder for a particular video codec.
#[cfg(feature = "exp-video-codecs")]
pub struct RegisteredVideoDecoder {
    /// Video codec details.
    pub codec: SupportedVideoCodec,
    /// Factory function to instantiate the video decoder.
    pub factory: VideoDecoderFactoryFn,
}

/// Registration details of a subtitle decoder for a particular subtitle codec.
#[cfg(feature = "exp-subtitle-codecs")]
pub struct RegisteredSubtitleDecoder {
    /// Subtitle codec details.
    pub codec: SupportedSubtitleCodec,
    /// Factory function to instantiate the subtitle decoder.
    pub factory: SubtitleDecoderFactoryFn,
}

struct InnerCodecRegistry<C, R> {
    preferred: HashMap<C, R>,
    standard: HashMap<C, R>,
    fallback: HashMap<C, R>,
}

impl<C, R> Default for InnerCodecRegistry<C, R> {
    fn default() -> Self {
        Self {
            preferred: Default::default(),
            standard: Default::default(),
            fallback: Default::default(),
        }
    }
}

impl<C, R> InnerCodecRegistry<C, R>
where
    C: Hash + std::cmp::Eq,
{
    fn get(&self, id: &C) -> Option<&R> {
        self.preferred.get(id).or_else(|| self.standard.get(id)).or_else(|| self.fallback.get(id))
    }

    /// Iterate over every registration across all tiers.
    fn iter(&self) -> impl Iterator<Item = &R> {
        self.preferred.values().chain(self.standard.values()).chain(self.fallback.values())
    }

    fn get_at_tier(&self, tier: Tier, id: &C) -> Option<&R> {
        match tier {
            Tier::Preferred => self.preferred.get(id),
            Tier::Standard => self.standard.get(id),
            Tier::Fallback => self.fallback.get(id),
        }
    }

    fn register_at_tier(&mut self, tier: Tier, id: C, reg: R) -> Option<R> {
        match tier {
            Tier::Preferred => self.preferred.insert(id, reg),
            Tier::Standard => self.standard.insert(id, reg),
            Tier::Fallback => self.fallback.insert(id, reg),
        }
    }
}

/// A `CodecRegistry` allows the registration of codecs, and provides a method to instantiate a
/// `Decoder` given a `CodecParameters` object.
#[derive(Default)]
pub struct CodecRegistry {
    audio: InnerCodecRegistry<AudioCodecId, RegisteredAudioDecoder>,
    #[cfg(feature = "exp-video-codecs")]
    video: InnerCodecRegistry<VideoCodecId, RegisteredVideoDecoder>,
    #[cfg(feature = "exp-subtitle-codecs")]
    subtitle: InnerCodecRegistry<SubtitleCodecId, RegisteredSubtitleDecoder>,
}

impl CodecRegistry {
    /// Instantiate a new `CodecRegistry`.
    pub fn new() -> Self {
        CodecRegistry {
            audio: Default::default(),
            #[cfg(feature = "exp-video-codecs")]
            video: Default::default(),
            #[cfg(feature = "exp-subtitle-codecs")]
            subtitle: Default::default(),
        }
    }

    /// Returns an allocation-free iterator over every audio decoder registered with this
    /// `CodecRegistry`, across all registration tiers.
    ///
    /// The same codec may appear more than once if it was registered at more than one tier.
    pub fn audio_decoders(&self) -> impl Iterator<Item = &RegisteredAudioDecoder> {
        self.audio.iter()
    }
}

/// Generates the lookup, registration, and instantiation methods of a `CodecRegistry` for one kind
/// of codec.
macro_rules! codec_registry_methods {
    (
        kind: $kind:literal,
        field: $field:ident,
        id_field: $id_field:ident,
        codec_id: $codec_id:ty,
        params: $params:ty,
        opts: $opts:ty,
        decoder: $decoder:path,
        registered: $registered:ident,
        registerable: $registerable:ident,
        get: $get:ident,
        get_at_tier: $get_at_tier:ident,
        register: $register:ident,
        register_at_tier: $register_at_tier:ident,
        make: $make:ident,
    ) => {
        #[doc = concat!(
            "Get the registration information of the most preferred ", $kind, " decoder for the \
             specified ", $kind, " codec."
        )]
        pub fn $get(&self, id: $codec_id) -> Option<&$registered> {
            self.$field.get(&id)
        }

        #[doc = concat!(
            "Get the registration information of the ", $kind, " decoder at the specified tier \
             for the specified ", $kind, " codec."
        )]
        pub fn $get_at_tier(&self, tier: Tier, id: $codec_id) -> Option<&$registered> {
            self.$field.get_at_tier(tier, &id)
        }

        #[doc = concat!(
            "Registers all ", $kind, " codecs supported by the ", $kind, " decoder at the \
             standard tier.\n\nIf a supported ", $kind, " codec was previously registered by \
             another ", $kind, " decoder at the same tier, it will be replaced within the \
             registry."
        )]
        pub fn $register<C: $registerable>(&mut self) {
            self.$register_at_tier::<C>(Tier::Standard);
        }

        #[doc = concat!(
            "Registers all ", $kind, " codecs supported by the ", $kind, " decoder at a \
             specific tier.\n\nIf a supported codec was previously registered by another ", $kind,
            " decoder at the same tier, it will be replaced within the registry."
        )]
        pub fn $register_at_tier<C: $registerable>(&mut self, tier: Tier) {
            for codec in C::supported_codecs() {
                let reg = $registered {
                    codec: *codec,
                    factory: |params, opts| C::try_registry_new(params, opts),
                };

                self.$field.register_at_tier(tier, codec.$id_field, reg);
            }
        }

        #[doc = concat!(
            "Instantiate a ", $kind, " decoder for the specified ", $kind, " codec \
             parameters.\n\nThis function searches the registry for a ", $kind, " decoder that \
             supports the codec. If one is found, it will be instantiated with the provided ",
            $kind, " codec parameters and ", $kind, " decoder options. If a suitable decoder \
             could not be found, or the decoder could not be instantiated, an error will be \
             returned."
        )]
        pub fn $make(&self, params: &$params, opts: &$opts) -> Result<Box<dyn $decoder>> {
            if let Some(codec) = self.$get(params.codec) {
                Ok((codec.factory)(params, opts)?)
            }
            else {
                // The error is static, so name the codec in the log for the user.
                log::warn!(
                    concat!("core (codec): no ", $kind, " decoder is registered for codec {}"),
                    params.codec
                );
                unsupported_error(concat!("core (codec): unsupported ", $kind, " codec"))
            }
        }
    };
}

impl CodecRegistry {
    codec_registry_methods! {
        kind: "audio",
        field: audio,
        id_field: id,
        codec_id: AudioCodecId,
        params: AudioCodecParameters,
        opts: AudioDecoderOptions,
        decoder: AudioDecoder,
        registered: RegisteredAudioDecoder,
        registerable: RegisterableAudioDecoder,
        get: get_audio_decoder,
        get_at_tier: get_audio_decoder_at_tier,
        register: register_audio_decoder,
        register_at_tier: register_audio_decoder_at_tier,
        make: make_audio_decoder,
    }
}

#[cfg(feature = "exp-video-codecs")]
impl CodecRegistry {
    codec_registry_methods! {
        kind: "video",
        field: video,
        id_field: codec,
        codec_id: VideoCodecId,
        params: VideoCodecParameters,
        opts: VideoDecoderOptions,
        decoder: VideoDecoder,
        registered: RegisteredVideoDecoder,
        registerable: RegisterableVideoDecoder,
        get: get_video_decoder,
        get_at_tier: get_video_decoder_at_tier,
        register: register_video_decoder,
        register_at_tier: register_video_decoder_at_tier,
        make: make_video_decoder,
    }
}

#[cfg(feature = "exp-subtitle-codecs")]
impl CodecRegistry {
    codec_registry_methods! {
        kind: "subtitle",
        field: subtitle,
        id_field: codec,
        codec_id: SubtitleCodecId,
        params: SubtitleCodecParameters,
        opts: SubtitleDecoderOptions,
        decoder: SubtitleDecoder,
        registered: RegisteredSubtitleDecoder,
        registerable: RegisterableSubtitleDecoded,
        get: get_subtitle_decoder,
        get_at_tier: get_subtitle_decoder_at_tier,
        register: register_subtitle_decoder,
        register_at_tier: register_subtitle_decoder_at_tier,
        make: make_subtitle_decoder,
    }
}

/// Convenience macro for declaring `CodecProfileInfo`.
#[macro_export]
macro_rules! codec_profile {
    ($id:expr, $short_name:expr, $long_name:expr) => {
        symphonia_core::codecs::CodecProfileInfo {
            profile: $id,
            short_name: $short_name,
            long_name: $long_name,
        }
    };
}

/// Convenience macro for declaring a `SupportedAudioCodec`.
#[macro_export]
macro_rules! support_audio_codec {
    ($id:expr, $short_name:expr, $long_name:expr) => {
        symphonia_core::codecs::registry::SupportedAudioCodec {
            id: $id,
            info: symphonia_core::codecs::CodecInfo {
                short_name: $short_name,
                long_name: $long_name,
                profiles: &[],
            },
        }
    };
    ($id:expr, $short_name:expr, $long_name:expr, $profiles:expr) => {
        symphonia_core::codecs::registry::SupportedAudioCodec {
            id: $id,
            info: symphonia_core::codecs::CodecInfo {
                short_name: $short_name,
                long_name: $long_name,
                profiles: $profiles,
            },
        }
    };
}

/// Convenience macro for declaring a `SupportedVideoCodec`.
#[macro_export]
macro_rules! support_video_codec {
    ($id:expr, $short_name:expr, $long_name:expr) => {
        symphonia_core::codecs::registry::SupportedVideoCodec {
            id: $id,
            info: symphonia_core::codecs::CodecInfo {
                short_name: $short_name,
                long_name: $long_name,
                profiles: &[],
            },
        }
    };
    ($id:expr, $short_name:expr, $long_name:expr, $profiles:expr) => {
        symphonia_core::codecs::registry::SupportedVideoCodec {
            id: $id,
            info: symphonia_core::codecs::CodecInfo {
                short_name: $short_name,
                long_name: $long_name,
                profiles: $profiles,
            },
        }
    };
}

/// Convenience macro for declaring a `SupportedSubtitleCodec`.
#[macro_export]
macro_rules! support_subtitle_codec {
    ($id:expr, $short_name:expr, $long_name:expr) => {
        symphonia_core::codecs::registry::SupportedSubtitleCodec {
            id: $id,
            info: symphonia_core::codecs::CodecInfo {
                short_name: $short_name,
                long_name: $long_name,
                profiles: &[],
            },
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::GenericAudioBufferRef;
    use crate::codecs::CodecInfo;
    use crate::codecs::audio::{AudioCodecId, CODEC_ID_NULL_AUDIO, FinalizeResult};
    use crate::errors::Result as SymResult;
    use crate::packet::PacketRef;

    struct DummyAudioDecoder(AudioCodecParameters);

    const DUMMY_CODEC_INFO: CodecInfo =
        CodecInfo { short_name: "dummy", long_name: "Dummy Codec", profiles: &[] };
    const DUMMY_CODEC_ID: AudioCodecId = CODEC_ID_NULL_AUDIO;

    impl AudioDecoder for DummyAudioDecoder {
        fn reset(&mut self) {}

        fn codec_info(&self) -> &CodecInfo {
            &DUMMY_CODEC_INFO
        }

        fn codec_params(&self) -> &AudioCodecParameters {
            &self.0
        }

        fn decode_ref(&mut self, _packet: &PacketRef<'_>) -> SymResult<GenericAudioBufferRef<'_>> {
            unimplemented!("not exercised by the enumeration test")
        }

        fn finalize(&mut self) -> FinalizeResult {
            unimplemented!()
        }

        fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
            unimplemented!()
        }
    }

    impl RegisterableAudioDecoder for DummyAudioDecoder {
        fn try_registry_new(
            params: &AudioCodecParameters,
            _opts: &AudioDecoderOptions,
        ) -> SymResult<Box<dyn AudioDecoder>> {
            Ok(Box::new(DummyAudioDecoder(params.clone())))
        }

        fn supported_codecs() -> &'static [SupportedAudioCodec] {
            &[SupportedAudioCodec { id: DUMMY_CODEC_ID, info: DUMMY_CODEC_INFO }]
        }
    }

    #[test]
    fn audio_decoders_enumerates_registered_codecs() {
        let mut registry = CodecRegistry::new();
        registry.register_audio_decoder::<DummyAudioDecoder>();

        let found: Vec<_> = registry.audio_decoders().collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].codec.info.short_name, "dummy");
        assert_eq!(found[0].codec.id, DUMMY_CODEC_ID);
    }
}
