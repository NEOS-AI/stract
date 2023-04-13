use criterion::{criterion_group, criterion_main, Criterion};
use optics::SiteRankings;
use stract::{
    index::Index,
    ranking::centrality_store::CentralityStore,
    searcher::{LocalSearcher, SearchQuery},
};

const INDEX_PATH: &str = "data/index";
const CENTRALITY_PATH: &str = "data/centrality";

macro_rules! bench {
    ($query:tt, $searcher:ident, $c:ident) => {
        let mut desc = "search '".to_string();
        desc.push_str($query);
        desc.push('\'');
        $c.bench_function(desc.as_str(), |b| {
            b.iter(|| {
                $searcher
                    .search(&SearchQuery {
                        query: $query.to_string(),
                        site_rankings: Some(SiteRankings {
                            liked: vec![
                                "docs.rs".to_string(),
                                "news.ycombinator.com".to_string(),
                                "pubmed.ncbi.nlm.nih.gov".to_string(),
                            ],
                            disliked: vec!["www.pinterest.com".to_string()],
                            blocked: vec![],
                        }),
                        ..Default::default()
                    })
                    .unwrap()
            })
        });
    };
}

pub fn criterion_benchmark(c: &mut Criterion) {
    let index = Index::open(INDEX_PATH).unwrap();
    let mut searcher = LocalSearcher::new(index);
    searcher.set_centrality_store(CentralityStore::open(CENTRALITY_PATH).into());

    for _ in 0..10 {
        bench!("the", searcher, c);
        bench!("dtu", searcher, c);
        bench!("the best", searcher, c);
        bench!("the circle of life", searcher, c);
    }
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
