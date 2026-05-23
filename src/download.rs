use std::collections::VecDeque;

struct DownloadScheduler {
    queue:Vec<VecDeque<String>>,

}

impl DownloadScheduler {
    fn new() -> Self {
        let queue = Vec::new();
        Self{
            queue
        }
    }

    
}
