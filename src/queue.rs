//! Queue that plays sounds one after the other.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::source::{Empty, SeekError, Source, Zero};
use crate::Sample;

use crate::common::{ChannelCount, SampleRate};
#[cfg(feature = "crossbeam-channel")]
use crossbeam_channel::{unbounded as channel, Receiver, Sender};
#[cfg(not(feature = "crossbeam-channel"))]
use std::sync::mpsc::{channel, Receiver, Sender};

/// Builds a new queue. It consists of an input and an output.
///
/// The input can be used to add sounds to the end of the queue, while the output implements
/// `Source` and plays the sounds.
///
/// The parameter indicates how the queue should behave if the queue becomes empty:
///
/// - If you pass `true`, then the queue is infinite and will play a silence instead until you add
///   a new sound.
/// - If you pass `false`, then the queue will report that it has finished playing.
///
pub fn queue(keep_alive_if_empty: bool) -> (Arc<SourcesQueueInput>, SourcesQueueOutput) {
    let input = Arc::new(SourcesQueueInput {
        next_sounds: Mutex::new(Vec::new()),
        keep_alive_if_empty: AtomicBool::new(keep_alive_if_empty),
    });

    let output = SourcesQueueOutput {
        current: Box::new(Empty::new()) as Box<_>,
        signal_after_end: None,
        input: input.clone(),
    };

    (input, output)
}

// TODO: consider reimplementing this with `from_factory`

type Sound = Box<dyn Source + Send>;
type SignalDone = Option<Sender<()>>;

/// The input of the queue.
pub struct SourcesQueueInput {
    next_sounds: Mutex<Vec<(Sound, SignalDone)>>,

    // See constructor.
    keep_alive_if_empty: AtomicBool,
}

impl SourcesQueueInput {
    /// Adds a new source to the end of the queue.
    #[inline]
    pub fn append<T>(&self, source: T)
    where
        T: Source + Send + 'static,
    {
        self.next_sounds
            .lock()
            .unwrap()
            .push((Box::new(source) as Box<_>, None));
    }

    /// Adds a new source to the end of the queue.
    ///
    /// The `Receiver` will be signalled when the sound has finished playing.
    ///
    /// Enable the feature flag `crossbeam-channel` in rodio to use a `crossbeam_channel::Receiver` instead.
    #[inline]
    pub fn append_with_signal<T>(&self, source: T) -> Receiver<()>
    where
        T: Source + Send + 'static,
    {
        let (tx, rx) = channel();
        self.next_sounds
            .lock()
            .unwrap()
            .push((Box::new(source) as Box<_>, Some(tx)));
        rx
    }

    /// Sets whether the queue stays alive if there's no more sound to play.
    ///
    /// See also the constructor.
    pub fn set_keep_alive_if_empty(&self, keep_alive_if_empty: bool) {
        self.keep_alive_if_empty
            .store(keep_alive_if_empty, Ordering::Release);
    }

    /// Removes all the sounds from the queue. Returns the number of sounds cleared.
    pub fn clear(&self) -> usize {
        let mut sounds = self.next_sounds.lock().unwrap();
        let len = sounds.len();
        sounds.clear();
        len
    }
}
/// The output of the queue. Implements `Source`.
pub struct SourcesQueueOutput {
    // The current iterator that produces samples.
    current: Box<dyn Source + Send>,

    // Signal this sender before picking from `next`.
    signal_after_end: Option<Sender<()>>,

    // The next sounds.
    input: Arc<SourcesQueueInput>,
}

const THRESHOLD: usize = 512;

impl Source for SourcesQueueOutput {
    #[inline]
    fn current_span_len(&self) -> Option<usize> {
        // This function is non-trivial because the boundary between two sounds in the queue should
        // be a span boundary as well.
        //
        // The current sound is free to return `None` for `current_span_len()`, in which case
        // we *should* return the number of samples remaining the current sound.
        // This can be estimated with `size_hint()`.
        //
        // If the `size_hint` is `None` as well, we are in the worst case scenario. To handle this
        // situation we force a span to have a maximum number of samples indicate by this
        // constant.

        // Try the current `current_span_len`.
        if let Some(val) = self.current.current_span_len() {
            if val != 0 {
                return Some(val);
            } else if self.input.keep_alive_if_empty.load(Ordering::Acquire)
                && self.input.next_sounds.lock().unwrap().is_empty()
            {
                // The next source will be a filler silence which will have the length of `THRESHOLD`
                return Some(THRESHOLD);
            }
        }

        // Try the size hint.
        let (lower_bound, _) = self.current.size_hint();
        // The iterator default implementation just returns 0.
        // That's a problematic value, so skip it.
        if lower_bound > 0 {
            return Some(lower_bound);
        }

        // Otherwise we use the constant value.
        Some(THRESHOLD)
    }

    #[inline]
    fn channels(&self) -> ChannelCount {
        self.current.channels()
    }

    #[inline]
    fn sample_rate(&self) -> SampleRate {
        self.current.sample_rate()
    }

    #[inline]
    fn total_duration(&self) -> Option<Duration> {
        None
    }

    /// Only seeks within the current source.
    // We can not go back to previous sources. We could implement seek such
    // that it advances the queue if the position is beyond the current song.
    //
    // We would then however need to enable seeking backwards across sources too.
    // That no longer seems in line with the queue behaviour.
    //
    // A final pain point is that we would need the total duration for the
    // next few songs.
    #[inline]
    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        self.current.try_seek(pos)
    }
}

impl Iterator for SourcesQueueOutput {
    type Item = Sample;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // Basic situation that will happen most of the time.
            if let Some(sample) = self.current.next() {
                return Some(sample);
            }

            // Since `self.current` has finished, we need to pick the next sound.
            // In order to avoid inlining this expensive operation, the code is in another function.
            if self.go_next().is_err() {
                return None;
            }
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.current.size_hint().0, None)
    }
}

impl SourcesQueueOutput {
    // Called when `current` is empty, and we must jump to the next element.
    // Returns `Ok` if the sound should continue playing, or an error if it should stop.
    //
    // This method is separate so that it is not inlined.
    fn go_next(&mut self) -> Result<(), ()> {
        if let Some(signal_after_end) = self.signal_after_end.take() {
            let _ = signal_after_end.send(());
        }

        let (next, signal_after_end) = {
            let mut next = self.input.next_sounds.lock().unwrap();

            if next.is_empty() {
                let silence = Box::new(Zero::new_samples(1, 44100, THRESHOLD)) as Box<_>;
                if self.input.keep_alive_if_empty.load(Ordering::Acquire) {
                    // Play a short silence in order to avoid spinlocking.
                    (silence, None)
                } else {
                    return Err(());
                }
            } else {
                next.remove(0)
            }
        };

        self.current = next;
        self.signal_after_end = signal_after_end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::buffer::SamplesBuffer;
    use crate::queue;
    use crate::source::Source;

    #[test]
    #[ignore] // FIXME: samples rate and channel not updated immediately after transition
    fn basic() {
        let (tx, mut rx) = queue::queue(false);

        tx.append(SamplesBuffer::new(1, 48000, vec![10.0, -10.0, 10.0, -10.0]));
        tx.append(SamplesBuffer::new(2, 96000, vec![5.0, 5.0, 5.0, 5.0]));

        assert_eq!(rx.channels(), 1);
        assert_eq!(rx.sample_rate(), 48000);
        assert_eq!(rx.next(), Some(10.0));
        assert_eq!(rx.next(), Some(-10.0));
        assert_eq!(rx.next(), Some(10.0));
        assert_eq!(rx.next(), Some(-10.0));
        assert_eq!(rx.channels(), 2);
        assert_eq!(rx.sample_rate(), 96000);
        assert_eq!(rx.next(), Some(5.0));
        assert_eq!(rx.next(), Some(5.0));
        assert_eq!(rx.next(), Some(5.0));
        assert_eq!(rx.next(), Some(5.0));
        assert_eq!(rx.next(), None);
    }

    #[test]
    fn immediate_end() {
        let (_, mut rx) = queue::queue(false);
        assert_eq!(rx.next(), None);
    }

    #[test]
    fn keep_alive() {
        let (tx, mut rx) = queue::queue(true);
        tx.append(SamplesBuffer::new(1, 48000, vec![10.0, -10.0, 10.0, -10.0]));

        assert_eq!(rx.next(), Some(10.0));
        assert_eq!(rx.next(), Some(-10.0));
        assert_eq!(rx.next(), Some(10.0));
        assert_eq!(rx.next(), Some(-10.0));

        for _ in 0..100000 {
            assert_eq!(rx.next(), Some(0.0));
        }
    }

    #[test]
    #[ignore] // TODO: not yet implemented
    fn no_delay_when_added() {
        let (tx, mut rx) = queue::queue(true);

        for _ in 0..500 {
            assert_eq!(rx.next(), Some(0.0));
        }

        tx.append(SamplesBuffer::new(1, 48000, vec![10.0, -10.0, 10.0, -10.0]));
        assert_eq!(rx.next(), Some(10.0));
        assert_eq!(rx.next(), Some(-10.0));
        assert_eq!(rx.next(), Some(10.0));
        assert_eq!(rx.next(), Some(-10.0));
    }
}
