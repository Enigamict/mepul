use std::collections::VecDeque;

use crate::{image_ref::ImageReference, registry::RegistryClient, types::Descriptor};

struct DownloadScheduler {
    queue: Vec<VecDeque<Descriptor>>,
}

impl DownloadScheduler {
    fn new() -> Self {
        let queue = Vec::new();
        Self { queue }
    }

    fn schedule(&mut self, mut layers: Vec<Descriptor>, image: &ImageReference) {
        layers.sort_by(|a, b| a.size.cmp(&b.size));

        for _ in layers.iter() {
            self.queue.push(VecDeque::new());
        }

        for i in layers.iter() {}
    }

    fn run() {}

    fn min(&self) -> Option<usize> {
        let min_index: Option<usize> = self
            .queue
            .iter()
            .enumerate()
            .min_by_key(|(_, deque)| deque.iter().map(|d| d.size).sum::<u64>())
            .map(|(index, _)| index);
        min_index
    }

    fn mean(layers: &Vec<Descriptor>) -> f32 {
        let total_sum: f32 = layers.iter().map(|desc| desc.size).sum::<u64>() as f32;
        let mean = total_sum / layers.len() as f32;

        mean
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_min_returns_index_of_queue_with_smallest_total_size() {
        let mut scheduler = DownloadScheduler::new();

        let mut queue_100 = VecDeque::new();
        queue_100.push_back(Descriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
            digest: "sha256:dummy1".to_string(),
            size: 100,
        });

        let mut queue_10000 = VecDeque::new();
        queue_10000.push_back(Descriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
            digest: "sha256:dummy2".to_string(),
            size: 10000,
        });

        let queue_empty: VecDeque<Descriptor> = VecDeque::new();

        scheduler.queue.push(queue_100);
        scheduler.queue.push(queue_10000);
        scheduler.queue.push(queue_empty);

        let result = scheduler.min();

        assert_eq!(result, Some(2));
    }
}