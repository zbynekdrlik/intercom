//! The N-1 mix-minus engine (issue 1345 M1).
//!
//! For every output channel of every participant, the engine sums the input channels routed to it
//! by the [`Matrix`] points, each scaled by its linear gain, accumulating in `f32` and clamping to
//! PCM16 at the end. Because the matrix REFUSES a `src == dst` point at load, a participant's own
//! source can never be routed back to it — the mix-minus (N-1) invariant holds structurally.
//!
//! Input/output are planar per participant: `[participant_id][channel_0based][sample]`. A missing
//! input channel (a participant sent fewer channels than a point references) contributes silence,
//! so the engine never panics on a short block.

use crate::matrix::Matrix;

/// One block of planar input audio, id-indexed. A participant with no input this block has an empty
/// channel vector.
#[derive(Debug, Clone)]
pub struct InputBlock {
    per_participant: Vec<Vec<Vec<i16>>>,
    frames: usize,
}

impl InputBlock {
    /// An all-silent input block for `n` participants of `frames` frames each.
    pub fn silent(n: usize, frames: usize) -> Self {
        InputBlock {
            per_participant: vec![Vec::new(); n],
            frames,
        }
    }

    /// Set one participant's planar input channels for this block. Channels shorter/longer than
    /// `frames` are tolerated at mix time (missing samples read as silence).
    pub fn set(&mut self, participant_id: usize, channels: Vec<Vec<i16>>) {
        self.per_participant[participant_id] = channels;
    }

    /// One sample of a participant's 1-based input channel, or `0` if that channel/sample is absent.
    #[inline]
    fn sample(&self, participant_id: usize, ch_1based: usize, i: usize) -> i16 {
        self.per_participant
            .get(participant_id)
            .and_then(|chans| chans.get(ch_1based.wrapping_sub(1)))
            .and_then(|ch| ch.get(i))
            .copied()
            .unwrap_or(0)
    }
}

/// One block of planar output audio, id-indexed: `[participant_id][channel_0based][sample]`.
#[derive(Debug, Clone)]
pub struct OutputBlock {
    per_participant: Vec<Vec<Vec<i16>>>,
}

impl OutputBlock {
    /// A participant's 1-based output channel samples, or `None` if that channel does not exist.
    pub fn channel(&self, participant_id: usize, ch_1based: usize) -> Option<&[i16]> {
        self.per_participant
            .get(participant_id)
            .and_then(|chans| chans.get(ch_1based.wrapping_sub(1)))
            .map(|v| v.as_slice())
    }

    /// A participant's output as interleaved PCM16 (channels interleaved per frame), for the VBAN
    /// sender. Returns an empty vec for a participant with no output channels.
    pub fn interleaved(&self, participant_id: usize, frames: usize) -> Vec<i16> {
        let Some(chans) = self.per_participant.get(participant_id) else {
            return Vec::new();
        };
        let n_ch = chans.len();
        if n_ch == 0 {
            return Vec::new();
        }
        let mut out = vec![0i16; frames * n_ch];
        for (c, ch) in chans.iter().enumerate() {
            for i in 0..frames {
                out[i * n_ch + c] = ch.get(i).copied().unwrap_or(0);
            }
        }
        out
    }
}

/// The mix engine over a loaded [`Matrix`].
pub struct Engine {
    matrix: Matrix,
}

impl Engine {
    pub fn new(matrix: Matrix) -> Self {
        Engine { matrix }
    }

    pub fn matrix(&self) -> &Matrix {
        &self.matrix
    }

    /// Number of participants (the id space for the input/output blocks).
    pub fn participant_count(&self) -> usize {
        self.matrix.participants.len()
    }

    /// Mix one block. Produces, for each participant, `out_channels` output channels of `frames`
    /// samples — the sum of every routed input channel scaled by its gain, clamped to PCM16.
    pub fn mix_block(&self, input: &InputBlock, frames: usize) -> OutputBlock {
        // f32 accumulators, id-indexed, one plane per declared output channel.
        let mut acc: Vec<Vec<Vec<f32>>> = self
            .matrix
            .participants
            .iter()
            .map(|p| vec![vec![0f32; frames]; p.out_channels])
            .collect();

        for point in &self.matrix.points {
            if point.mute {
                continue;
            }
            let dst_plane = &mut acc[point.dst][point.out_ch - 1];
            for (i, slot) in dst_plane.iter_mut().enumerate().take(frames) {
                let s = input.sample(point.src, point.in_ch, i);
                *slot += s as f32 * point.gain_linear;
            }
        }

        let per_participant = acc
            .into_iter()
            .map(|planes| {
                planes
                    .into_iter()
                    .map(|plane| {
                        plane
                            .into_iter()
                            .map(|v| v.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16)
                            .collect()
                    })
                    .collect()
            })
            .collect();

        OutputBlock { per_participant }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::Matrix;

    /// A fully-crossed 3-participant N-1 matrix: each of p1/p2/p3 has a 1-ch mic in + a 1-ch
    /// talkback out; every mic routes to every OTHER participant's out (no self-route).
    fn n1_three() -> Matrix {
        let toml = r#"
[hub]
bind = "0.0.0.0:8790"
vban_bind = "0.0.0.0:6980"
sample_rate = 48000
block_frames = 4

[[participant]]
name = "p1"
role = "cambox"
adapter = "vban"
host = "p1.lan"
in_stream = "p1"
out_stream = "p1"
in_channels = 1
out_channels = 1

[[participant]]
name = "p2"
role = "cambox"
adapter = "vban"
host = "p2.lan"
in_stream = "p2"
out_stream = "p2"
in_channels = 1
out_channels = 1

[[participant]]
name = "p3"
role = "cambox"
adapter = "vban"
host = "p3.lan"
in_stream = "p3"
out_stream = "p3"
in_channels = 1
out_channels = 1

[[point]]
src = "p1"
in_ch = 1
dst = "p2"
out_ch = 1
[[point]]
src = "p1"
in_ch = 1
dst = "p3"
out_ch = 1
[[point]]
src = "p2"
in_ch = 1
dst = "p1"
out_ch = 1
[[point]]
src = "p2"
in_ch = 1
dst = "p3"
out_ch = 1
[[point]]
src = "p3"
in_ch = 1
dst = "p1"
out_ch = 1
[[point]]
src = "p3"
in_ch = 1
dst = "p2"
out_ch = 1
"#;
        Matrix::from_toml(toml).unwrap()
    }

    #[test]
    fn n1_output_is_the_sum_of_all_but_own_source_sample_exact() {
        let engine = Engine::new(n1_three());
        let (p1, p2, p3) = (
            engine.matrix().id_of("p1").unwrap(),
            engine.matrix().id_of("p2").unwrap(),
            engine.matrix().id_of("p3").unwrap(),
        );
        let frames = 4;
        let mut input = InputBlock::silent(engine.participant_count(), frames);
        // Distinct constant tones so a leak is obvious.
        input.set(p1, vec![vec![100, 100, 100, 100]]);
        input.set(p2, vec![vec![200, 200, 200, 200]]);
        input.set(p3, vec![vec![300, 300, 300, 300]]);

        let out = engine.mix_block(&input, frames);

        // p1 hears p2 + p3 = 500, NEVER its own 100.
        assert_eq!(out.channel(p1, 1).unwrap(), &[500, 500, 500, 500]);
        // p2 hears p1 + p3 = 400.
        assert_eq!(out.channel(p2, 1).unwrap(), &[400, 400, 400, 400]);
        // p3 hears p1 + p2 = 300.
        assert_eq!(out.channel(p3, 1).unwrap(), &[300, 300, 300, 300]);
    }

    #[test]
    fn missing_input_channel_reads_as_silence() {
        // p1 sends NO input this block; p2/p3 do. p1's own out is p2+p3; the others' contributions
        // from p1 are silent (0), so no panic and no phantom energy.
        let engine = Engine::new(n1_three());
        let (p1, p2, p3) = (
            engine.matrix().id_of("p1").unwrap(),
            engine.matrix().id_of("p2").unwrap(),
            engine.matrix().id_of("p3").unwrap(),
        );
        let frames = 4;
        let mut input = InputBlock::silent(engine.participant_count(), frames);
        input.set(p2, vec![vec![10, 10, 10, 10]]);
        input.set(p3, vec![vec![20, 20, 20, 20]]);
        let out = engine.mix_block(&input, frames);
        assert_eq!(out.channel(p1, 1).unwrap(), &[30, 30, 30, 30]);
        // p2 hears p1(silent) + p3 = 20.
        assert_eq!(out.channel(p2, 1).unwrap(), &[20, 20, 20, 20]);
    }

    #[test]
    fn output_clamps_to_pcm16_range() {
        let engine = Engine::new(n1_three());
        let (p1, p2, p3) = (
            engine.matrix().id_of("p1").unwrap(),
            engine.matrix().id_of("p2").unwrap(),
            engine.matrix().id_of("p3").unwrap(),
        );
        let frames = 2;
        let mut input = InputBlock::silent(engine.participant_count(), frames);
        input.set(p2, vec![vec![30000, 30000]]);
        input.set(p3, vec![vec![30000, 30000]]);
        // p1 = 60000 → clamped to i16::MAX.
        let out = engine.mix_block(&input, frames);
        assert_eq!(out.channel(p1, 1).unwrap(), &[i16::MAX, i16::MAX]);
    }

    #[test]
    fn interleaves_stereo_output() {
        // A synthetic 2-out-channel participant to prove interleaving order (L,R per frame).
        let toml = r#"
[hub]
bind = "0.0.0.0:8790"
vban_bind = "0.0.0.0:6980"
sample_rate = 48000
block_frames = 2

[[participant]]
name = "src"
role = "cambox"
adapter = "vban"
host = "src.lan"
in_stream = "src"
in_channels = 2
out_channels = 0

[[participant]]
name = "cam1"
role = "cambox"
adapter = "vban"
host = "cam1.lan"
out_stream = "cam1"
in_channels = 0
out_channels = 2

[[point]]
src = "src"
in_ch = 1
dst = "cam1"
out_ch = 1
[[point]]
src = "src"
in_ch = 2
dst = "cam1"
out_ch = 2
"#;
        let engine = Engine::new(Matrix::from_toml(toml).unwrap());
        let src = engine.matrix().id_of("src").unwrap();
        let cam1 = engine.matrix().id_of("cam1").unwrap();
        let frames = 2;
        let mut input = InputBlock::silent(engine.participant_count(), frames);
        input.set(src, vec![vec![1, 2], vec![3, 4]]); // L=[1,2], R=[3,4]
        let out = engine.mix_block(&input, frames);
        // Interleaved: frame0 L,R ; frame1 L,R = [1,3, 2,4].
        assert_eq!(out.interleaved(cam1, frames), vec![1, 3, 2, 4]);
    }
}
