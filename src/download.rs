use std::marker::PhantomData;
use std::{collections::VecDeque, fmt::Debug};

use crate::{image_ref::ImageReference, registry::RegistryClient, types::Descriptor};

struct DownloadScheduler {
    queue: Vec<VecDeque<Box<dyn TraitDownloadedInfo>>>,
}

trait TraitDownloadedInfo {
    fn size(&self) -> u64;
    fn fmt_debug(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result;
}

impl std::fmt::Debug for dyn TraitDownloadedInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.fmt_debug(f)
    }
}

struct SplitDownloadedInfo<F: TraitDownloadedInfo + ?Sized> {
    start: u64,
    end: u64,
    size: u64,
    registry: String,
    repository: String,
    digest: String,
    _PhantomData: PhantomData<F>,
}

struct DownloadedInfo<F: TraitDownloadedInfo + ?Sized> {
    start: u64,
    end: u64,
    size: u64,
    registry: String,
    repository: String,
    digest: String,
    _PhantomData: PhantomData<F>,
}

impl<F: TraitDownloadedInfo + ?Sized> TraitDownloadedInfo for SplitDownloadedInfo<F> {
    fn size(&self) -> u64 {
        self.end - self.start 
    }
    fn fmt_debug(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        <Self as std::fmt::Debug>::fmt(self, f)
    }
}

impl<F: TraitDownloadedInfo + ?Sized> TraitDownloadedInfo for DownloadedInfo<F> {
    fn size(&self) -> u64 {
        self.end - self.start
    }
    fn fmt_debug(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        <Self as std::fmt::Debug>::fmt(self, f)
    }
}

impl<F: TraitDownloadedInfo + ?Sized> std::fmt::Debug for SplitDownloadedInfo<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SplitDownloadedInfo")
            .field("start", &self.start)
            .field("end", &self.end)
            .field("size", &self.size)
            .field("registry", &self.registry)
            .field("repository", &self.repository)
            .field("digest", &self.digest)
            .finish()
    }
}

impl<F: TraitDownloadedInfo + ?Sized> std::fmt::Debug for DownloadedInfo<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadedInfo") 
            .field("start", &self.start)
            .field("end", &self.end)
            .field("size", &self.size)
            .field("registry", &self.registry)
            .field("repository", &self.repository)
            .field("digest", &self.digest)
            .finish()
    }
}

impl TraitDownloadedInfo for Descriptor {
    fn size(&self) -> u64 {
        self.size
    }
    fn fmt_debug(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Descriptor(size: {}, digest: {})",
            self.size, self.digest
        )
    }
}

impl DownloadScheduler {
    fn new(k:usize) -> Self {
        let mut queue = Vec::new();
          for _ in 0..k {
            queue.push(VecDeque::new());
        }
        Self { queue }
    }

    fn schedule(&mut self, mut layers: Vec<Descriptor>, image: &ImageReference) {
        layers.sort_by(|a, b| a.size.cmp(&b.size));
        let mean_layers_size = Self::get_mean_layers_size(&mut layers);

        // for _ in layers.iter() {
        //     self.queue.push(VecDeque::new());
        // }

        for layer in layers.iter() {
            if layer.size as f32 > mean_layers_size {
                let min_queue_index = self.get_min_size_queue_index();
                let (l1, l2) = self.splite_layer(&image, &layer);
                self.queue[min_queue_index.unwrap()].push_back(Box::new(l1));
                let min_queue_index = self.get_min_size_queue_index().unwrap();
                self.queue[min_queue_index].push_back(Box::new(l2));
            } else {
                let min_queue_index = self.get_min_size_queue_index();

                self.queue[min_queue_index.unwrap()].push_back(Box::new(DownloadedInfo::<
                    dyn TraitDownloadedInfo,
                > {
                    start: 0,
                    end: layer.size,
                    size: layer.size,
                    registry: image.registry.clone(),
                    repository: image.repository.clone(),
                    digest: layer.digest.clone(),
                    _PhantomData: PhantomData,
                }));
            }
        }
    }

    fn get_min_size_queue_index(&self) -> Option<usize> {
        let min_index: Option<usize> = self
            .queue
            .iter()
            .enumerate()
            .min_by_key(|(_, deque)| deque.iter().map(|d| d.size()).sum::<u64>())
            .map(|(index, _)| index);
        min_index
    }

    fn _get_min_size_queue_index(&self) -> Option<usize> {
        let min_index: Option<usize> = self
            .queue
            .iter()
            .enumerate()
            .min_by_key(|(_, deque)| deque.iter().map(|d| d.size()).sum::<u64>())
            .map(|(index, _)| index);
        min_index
    }

    fn get_mean_layers_size(layers: &Vec<Descriptor>) -> f32 {
        let total_sum: f32 = layers.iter().map(|desc| desc.size).sum::<u64>() as f32;
        let mean = total_sum / layers.len() as f32;

        mean
    }

    fn splite_layer(
        &self,
        image: &ImageReference,
        layer: &Descriptor,
    ) -> (
        SplitDownloadedInfo<dyn TraitDownloadedInfo>,
        SplitDownloadedInfo<dyn TraitDownloadedInfo>,
    ) {
        let start1 = 0;
        let end1 = layer.size / 2;

        let start2 = end1;
        let end2 = layer.size;

        let l1 = SplitDownloadedInfo {
            start: start1,
            end: end1,
            size: start1 + end1,
            registry: image.registry.clone(),
            repository: image.repository.clone(),
            digest: layer.digest.clone(),
            _PhantomData: PhantomData,
        };

        let l2 = SplitDownloadedInfo {
            start: start2,
            end: end2,
            size: start2 + end2,
            registry: image.registry.clone(),
            repository: image.repository.clone(),
            digest: layer.digest.clone(),
            _PhantomData: PhantomData,
        };

        return (l1, l2);
    }

    fn concat_layer(&self) {}

    fn run() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_min_returns_index_of_queue_with_smallest_total_size() {
        let mut scheduler = DownloadScheduler::new(2);

        let mut queue_100: VecDeque<Box<dyn TraitDownloadedInfo>> = VecDeque::new();
        queue_100.push_back(Box::new(Descriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
            digest: "sha256:dummy1".to_string(),
            size: 100,
        }));

        let mut queue_10000: VecDeque<Box<dyn TraitDownloadedInfo>> = VecDeque::new();
        queue_10000.push_back(Box::new(Descriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
            digest: "sha256:dummy2".to_string(),
            size: 10000,
        }));

        let queue_empty: VecDeque<Box<dyn TraitDownloadedInfo>> = VecDeque::new();

        scheduler.queue.push(queue_100);
        scheduler.queue.push(queue_10000);
        scheduler.queue.push(queue_empty);

        let result = scheduler.get_min_size_queue_index();

        //assert_eq!(result, Some(2));
    }

    #[test]
    fn test_schedule() {
        let mut scheduler = DownloadScheduler::new(3);

        let mut layers = Vec::new();
        layers.push(Descriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
            digest: "sha256:dummy1".to_string(),
            size: 1,
        });

        layers.push(Descriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
            digest: "sha256:dummy2".to_string(),
            size: 100,
        });

        layers.push(Descriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
            digest: "sha256:dummy3".to_string(),
            size: 100,
        });

        layers.push(Descriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
            digest: "sha256:dummy4".to_string(),
            size: 10000,
        });

        let image = &ImageReference {
            registry: "takata_registry".to_string(),
            repository: "takata_repository".to_string(),
            reference: "takata_reference".to_string(),
        };

        scheduler.schedule(layers, image);

        scheduler
            .queue
            .iter()
            .enumerate()
            .for_each(|(index, queue)| {
                let total_size: u64 = queue.iter().map(|item| item.size()).sum();
                println!("Queue: {:?}", queue);

                println!("Queue [{}] total size: {}", index, total_size);
            });
    }
}
